//! Per-monitor lock: an opaque shade over one output while the rest of the
//! desktop keeps working.
//!
//! The session lock ([`Jwm::lock_screen`]) owns the whole seat: it grabs the
//! keyboard and the pointer, and the compositor paints over every output.
//! That is the wrong tool for the screen in the corner showing something the
//! room should not see while the user keeps typing on the other one, which is
//! what this is for.
//!
//! What a locked monitor gets:
//!
//! - an opaque shade drawn above everything on that output — clients, the
//!   status bar, the workspace overlays — and drawn before the frame is
//!   captured, so screenshots, recordings and the remote viewer see the shade
//!   too;
//! - no focus: the selection cannot move onto it, a client on it cannot be
//!   focused, and a window mapping there does not steal the keyboard;
//! - no pointer: button presses over the shade are swallowed rather than
//!   delivered to whatever is invisible underneath;
//! - a password: the shade comes off through the same PAM path the session
//!   lock uses, on a card drawn on that monitor alone.
//!
//! What it deliberately is *not*: a security boundary for the session. The
//! seat around it is unlocked — whoever is at the keyboard can still use every
//! other monitor, and everything a logged-in user can do. It hides one
//! screen's contents from the room; [`Jwm::lock_screen`] is what locks the
//! session.
//!
//! The shade is the compositor's job, so this refuses to lock without one:
//! recording state that nothing draws would leave a monitor the user believes
//! is covered showing every window on it.

use crate::backend::api::{Backend, MonitorShade};
use crate::core::models::ClientKey;
use crate::jwm::Jwm;
use crate::jwm::MonitorKey;
use crate::jwm::types::WMArgEnum;

/// One locked output, remembered with the rectangle it had when it was
/// locked.
///
/// The rectangle is not decoration: monitor numbers are positional, so a
/// display change can hand number 1 to a different physical screen. A lock
/// whose output moved, resized or went away is dropped rather than silently
/// re-pointed at whatever inherited the number — see
/// [`MonitorLockState::retain_outputs`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockedMonitor {
    pub num: i32,
    pub rect: (i32, i32, i32, i32),
}

/// Which monitors are locked, newest last.
#[derive(Debug, Default)]
pub struct MonitorLockState {
    locked: Vec<LockedMonitor>,
}

impl MonitorLockState {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locked.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.locked.len()
    }

    #[must_use]
    pub fn contains(&self, num: i32) -> bool {
        self.locked.iter().any(|entry| entry.num == num)
    }

    /// The monitor numbers currently locked, in the order they were locked.
    pub fn nums(&self) -> impl Iterator<Item = i32> + '_ {
        self.locked.iter().map(|entry| entry.num)
    }

    /// The most recently locked monitor: what an unlock with no monitor
    /// argument means.
    #[must_use]
    pub fn latest(&self) -> Option<i32> {
        self.locked.last().map(|entry| entry.num)
    }

    /// Record a lock. `false` when that monitor was already locked, which
    /// leaves the original entry — and so the original lock order — alone.
    pub fn lock(&mut self, num: i32, rect: (i32, i32, i32, i32)) -> bool {
        if self.contains(num) {
            return false;
        }
        self.locked.push(LockedMonitor { num, rect });
        true
    }

    /// Lift one lock. `false` when that monitor was not locked.
    pub fn unlock(&mut self, num: i32) -> bool {
        let before = self.locked.len();
        self.locked.retain(|entry| entry.num != num);
        self.locked.len() != before
    }

    /// Lift the oldest lock, reporting which monitor it was on.
    ///
    /// The invariant "one monitor always stays unlocked" has to survive a
    /// display change, and the oldest lock is the one whose reason is most
    /// likely spent.
    pub fn drop_oldest(&mut self) -> Option<i32> {
        (!self.locked.is_empty()).then(|| self.locked.remove(0).num)
    }

    /// Lift every lock, reporting what came off.
    pub fn clear(&mut self) -> Vec<i32> {
        std::mem::take(&mut self.locked)
            .into_iter()
            .map(|entry| entry.num)
            .collect()
    }

    /// The payload the compositor draws.
    #[must_use]
    pub fn shades(&self) -> Vec<MonitorShade> {
        self.locked
            .iter()
            .filter_map(|entry| {
                let (x, y, width, height) = entry.rect;
                MonitorShade::new(entry.num, x, y, width, height)
            })
            .collect()
    }

    /// Drop every lock whose output is gone or is no longer the rectangle it
    /// was locked at, and report which monitor numbers came off.
    ///
    /// Keeping a lock keyed on a number alone across a display change is how
    /// a shade ends up on the wrong screen — and a shade on the wrong screen
    /// means the screen the user locked is the one now showing. Dropping is
    /// the honest answer: it is visible, it is logged, and re-locking is one
    /// key away.
    pub fn retain_outputs(&mut self, present: &[(i32, (i32, i32, i32, i32))]) -> Vec<i32> {
        let mut dropped = Vec::new();
        self.locked.retain(|entry| {
            let kept = present
                .iter()
                .any(|(num, rect)| *num == entry.num && *rect == entry.rect);
            if !kept {
                dropped.push(entry.num);
            }
            kept
        });
        dropped
    }
}

impl Jwm {
    /// Whether this monitor number is behind a lock shade.
    pub(crate) fn monitor_is_locked(&self, num: i32) -> bool {
        self.features.monitor_lock.contains(num)
    }

    /// Whether this monitor is behind a lock shade.
    pub(crate) fn monitor_key_is_locked(&self, key: MonitorKey) -> bool {
        if self.features.monitor_lock.is_empty() {
            return false;
        }
        self.state
            .monitors
            .get(key)
            .is_some_and(|monitor| self.monitor_is_locked(monitor.num))
    }

    /// Whether this client sits on a locked monitor — and so may not take
    /// focus, because nobody can see it.
    pub(crate) fn client_is_on_locked_monitor(&self, client: ClientKey) -> bool {
        if self.features.monitor_lock.is_empty() {
            return false;
        }
        self.state
            .clients
            .get(client)
            .and_then(|client| client.mon)
            .is_some_and(|key| self.monitor_key_is_locked(key))
    }

    /// The locked monitor under this point in global coordinates, if any.
    pub(crate) fn locked_monitor_at(&self, x: f64, y: f64) -> Option<i32> {
        if self.features.monitor_lock.is_empty() {
            return None;
        }
        self.features
            .monitor_lock
            .shades()
            .into_iter()
            .find(|shade| shade.contains(x, y))
            .map(|shade| shade.num)
    }

    /// Whether a point in global coordinates is under a lock shade.
    pub(crate) fn point_is_locked(&self, x: f64, y: f64) -> bool {
        self.locked_monitor_at(x, y).is_some()
    }

    /// Push the current shades to the compositor. Cheap and idempotent: it is
    /// called from every path that can change the set, including the monitor
    /// refresh, so a compositor that restarted gets its shades back.
    pub(crate) fn sync_monitor_shades(&mut self, backend: &mut dyn Backend) {
        let shades = self.features.monitor_lock.shades();
        backend.compositor_set_monitor_shades(&shades);
        backend.compositor_force_full_redraw();
    }

    /// The rectangle of a locked monitor, for the unlock prompt's viewport.
    pub(crate) fn locked_monitor_rect(&self, num: i32) -> Option<(i32, i32, i32, i32)> {
        if !self.monitor_is_locked(num) {
            return None;
        }
        let key = self.get_monitor_by_id(num)?;
        self.state.monitors.get(key).map(|monitor| {
            (
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            )
        })
    }

    /// The monitor an argument names: a number, or "the one I am on" for
    /// anything negative (what a key binding sends).
    ///
    /// "The one I am on" is the selected monitor — except over a shade. The
    /// selection can never be on a locked monitor, which is the whole point
    /// of one, so that is the single place where the pointer and the
    /// selection disagree; and it is exactly where the user means the screen
    /// they are pointing at. Reading the pointer there is what makes the key
    /// a toggle: move onto the shade, press it, and the card that lifts the
    /// shade comes up on it.
    fn monitor_arg(
        &self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<i32, Box<dyn std::error::Error>> {
        let requested = match *arg {
            WMArgEnum::Int(num) => num,
            WMArgEnum::UInt(num) => i32::try_from(num).unwrap_or(-1),
            _ => -1,
        };
        if requested >= 0 {
            return Ok(requested);
        }
        if !self.features.monitor_lock.is_empty() {
            let (x, y) = backend
                .input_ops()
                .get_pointer_position()
                .unwrap_or(self.last_mouse_root);
            if let Some(num) = self.locked_monitor_at(x, y) {
                return Ok(num);
            }
        }
        let selected = self
            .state
            .sel_mon
            .and_then(|key| self.state.monitors.get(key))
            .map(|monitor| monitor.num);
        selected.ok_or_else(|| "no monitor is selected".into())
    }

    /// Lock one monitor: cover it with an opaque shade and keep focus, the
    /// pointer and new windows off it until the password lifts it.
    ///
    /// `Int(n)` locks monitor `n`; anything negative — what the key binding
    /// sends — means the monitor in use, or the one under the pointer while
    /// that is a shade (see [`Self::monitor_arg`]). Naming a monitor that is
    /// already locked opens its unlock prompt instead, so the one key both
    /// locks a screen and asks for the password that lifts it.
    pub fn lock_monitor(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // A lock card is modal and owns the keyboard. Reachable over IPC
        // while one is up, never from a key.
        if self.features.system_ui.is_session_lock() {
            return Err("the session is locked".into());
        }
        let target = self.monitor_arg(backend, arg)?;
        if self.monitor_is_locked(target) {
            return self.unlock_monitor(backend, &WMArgEnum::Int(target));
        }
        let Some(monitor_key) = self.get_monitor_by_id(target) else {
            return Err(format!("no monitor {target}").into());
        };
        // One monitor is the whole session: a shade over it is a black screen
        // with an unlocked keyboard behind it, which is worse than either
        // thing on its own. The same argument holds for the last unlocked
        // monitor of several.
        let monitors = self.state.monitor_order.len();
        if monitors < 2 {
            return Err("locking a single monitor needs a second output; use lock_screen".into());
        }
        if self.features.monitor_lock.len() + 1 >= monitors {
            return Err(
                "that is the last unlocked monitor; use lock_screen to lock the session".into(),
            );
        }
        // Nothing draws the shade without a compositor, and a lock the user
        // cannot see is a monitor they believe is covered and is not.
        if !backend.has_compositor() {
            return Err("locking a monitor requires the JWM compositor".into());
        }
        let Some(rect) = self.state.monitors.get(monitor_key).map(|monitor| {
            (
                monitor.geometry.m_x,
                monitor.geometry.m_y,
                monitor.geometry.m_w,
                monitor.geometry.m_h,
            )
        }) else {
            return Err(format!("no monitor {target}").into());
        };
        if rect.2 <= 0 || rect.3 <= 0 {
            return Err(format!("monitor {target} has no usable geometry").into());
        }

        // Recorded first, before anything below can act on it. The panel
        // close may hand back a compositor that was leased for that panel,
        // and the lease guard keeps it exactly because a monitor is locked —
        // so the lock has to be on the books by then. It also puts the
        // focus refusals in force for the move that follows.
        self.features.monitor_lock.lock(target, rect);

        // Expose and the tag overview draw the windows they were entered
        // with and never re-plan: the expose grid would keep this monitor's
        // windows on show across the unlocked outputs, and an overview entered
        // here would stay on it under the shade, still holding the keyboard.
        self.leave_modes_showing_locked_windows(backend);

        // A panel drawn on the monitor going dark would end up under the
        // shade: invisible, still holding the keyboard and the pointer, and
        // dismissible only by a key the user cannot see the target of. Shell
        // panels are drawn on the selected monitor, so that is the one test.
        if self.features.system_ui.is_active() && self.state.sel_mon == Some(monitor_key) {
            self.close_system_ui(backend);
        }

        if self.state.sel_mon == Some(monitor_key) {
            let moved = match self.first_unlocked_monitor() {
                Some(next) => self.switch_to_monitor(backend, next),
                // Unreachable while the last-unlocked refusal above holds;
                // dropping focus is still the right answer if it ever is.
                None => self.focus(backend, None),
            };
            if let Err(error) = moved {
                // The shade still goes up. A monitor that is covered while
                // still selected is recoverable — the next focus call moves
                // off it — while an uncovered monitor the user believes is
                // locked is not.
                log::warn!("Monitor {target}: could not move the selection off it: {error}");
            }
        }
        self.sync_monitor_shades(backend);
        log::info!("Monitor {target} locked");
        self.broadcast_ipc_event(
            "monitor/lock",
            serde_json::json!({ "monitor": target, "locked": true }),
        );
        Ok(())
    }

    /// Ask for the password that lifts a monitor's shade.
    ///
    /// `Int(n)` asks for monitor `n`; anything negative asks for the most
    /// recently locked one, which is what the key binding and a bare
    /// `jwm-tool msg unlock_monitor` mean. The card is drawn on the locked
    /// monitor and is modal while it is up — Escape hands the keyboard back
    /// and leaves the shade exactly where it was.
    pub fn unlock_monitor(
        &mut self,
        backend: &mut dyn Backend,
        arg: &WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.features.system_ui.is_session_lock() {
            return Err("the session is locked".into());
        }
        if self.features.monitor_lock.is_empty() {
            return Err("no monitor is locked".into());
        }
        let requested = match *arg {
            WMArgEnum::Int(num) => num,
            WMArgEnum::UInt(num) => i32::try_from(num).unwrap_or(-1),
            _ => -1,
        };
        let target = if requested >= 0 {
            requested
        } else {
            self.features
                .monitor_lock
                .latest()
                .ok_or_else(|| -> Box<dyn std::error::Error> { "no monitor is locked".into() })?
        };
        if !self.monitor_is_locked(target) {
            return Err(format!("monitor {target} is not locked").into());
        }
        // Already asking for this one: pressing the key again is not a
        // second prompt, and it must not close the one on screen either —
        // that is Escape's job.
        if self.features.system_ui.monitor_lock_target() == Some(target) {
            return Ok(());
        }
        // Another monitor's prompt is up. `prepare_system_ui` refuses to
        // replace any lock card, so this one comes down first.
        if self.features.system_ui.monitor_lock_target().is_some() {
            self.close_system_ui(backend);
        }
        self.prepare_system_ui(
            backend,
            "monitor unlock",
            crate::jwm::features::toggles::SystemUiPointerGrab::Buttons,
        )?;
        self.features.system_ui = crate::jwm::features::SystemUiState::monitor_lock(target);
        self.sync_system_ui(backend);
        Ok(())
    }

    /// Take one monitor's shade down. `false` when it was not locked.
    ///
    /// The single place a lock comes off for good: the authenticated unlock,
    /// a display change that invalidated it, and the shutdown path all land
    /// here so the push, the log line and the event cannot drift apart.
    pub(crate) fn lift_monitor_lock(&mut self, backend: &mut dyn Backend, num: i32) -> bool {
        if !self.features.monitor_lock.unlock(num) {
            return false;
        }
        self.sync_monitor_shades(backend);
        log::info!("Monitor {num} unlocked");
        self.broadcast_ipc_event(
            "monitor/lock",
            serde_json::json!({ "monitor": num, "locked": false }),
        );
        // The prompt may have been the only reason a compositor was leased.
        self.release_lease_after_last_lock(backend);
        true
    }

    /// Hand back a compositor that a panel leased and that was kept only for
    /// the shades, once the last shade is down.
    ///
    /// The lease survives the panel exactly because shades need the renderer
    /// (`release_temporary_system_ui_compositor` refuses while any monitor is
    /// locked), so every path that takes the last lock off — the unlock and a
    /// display change alike — has to return it here or nothing will. Not
    /// while a panel is on screen: it may be drawing on that same lease, and
    /// its own close hands it back now that no monitor is locked.
    fn release_lease_after_last_lock(&mut self, backend: &mut dyn Backend) {
        if self.features.monitor_lock.is_empty() && !self.features.system_ui.is_active() {
            self.release_temporary_system_ui_compositor(backend, "monitor lock");
        }
    }

    /// Take down expose and the tag overview where they would show a locked
    /// monitor's windows.
    ///
    /// Both are entered with the windows of unlocked monitors and neither
    /// re-plans afterwards — the locked-monitor filter in `toggle_expose`
    /// runs only on entry. Expose spreads its thumbnails over the whole
    /// desktop, so any lock while it is up can leave a covered window on
    /// show elsewhere, and it simply exits. The overview's prism sits on the
    /// monitor it was entered on; it comes down only when its windows are on
    /// a locked one, where it would be under the shade and still swallowing
    /// every key. The same teardown `prepare_system_ui` performs.
    fn leave_modes_showing_locked_windows(&mut self, backend: &mut dyn Backend) {
        if self.features.overview.active
            && self
                .features
                .overview
                .clients
                .iter()
                .any(|&client| self.client_is_on_locked_monitor(client))
        {
            self.features.overview.deactivate();
            backend.compositor_set_overview_mode(false, &[]);
            let _ = backend.key_ops().ungrab_keyboard();
        }
        if self.features.expose_active
            && let Err(error) = self.apply_expose_action(
                backend,
                crate::jwm::features::expose_plan::ExposeAction::Exit { focus: None },
            )
        {
            // The lock is already on the books and its shade still goes up;
            // an exit that could not refocus is not a reason to undo it.
            log::warn!("Monitor lock: expose did not exit cleanly: {error}");
        }
    }

    /// Whether one more monitor could go behind a shade: two outputs at
    /// least, and one of them staying unlocked afterwards.
    ///
    /// The compositor is deliberately not part of this. It answers for the
    /// control center's row, and that panel is itself compositor-drawn — by
    /// the time the row can be seen or clicked, a compositor is running.
    /// [`Self::lock_monitor`] re-checks for the callers that are not a panel,
    /// and says which of the two reasons refused them.
    pub(crate) fn can_lock_another_monitor(&self) -> bool {
        let monitors = self.state.monitor_order.len();
        self.state.sel_mon.is_some()
            && monitors >= 2
            && self.features.monitor_lock.len() + 1 < monitors
    }

    /// The first monitor in layout order that is not locked.
    pub(crate) fn first_unlocked_monitor(&self) -> Option<MonitorKey> {
        self.state
            .monitor_order
            .iter()
            .copied()
            .find(|&key| !self.monitor_key_is_locked(key))
    }

    /// Drop the locks a display change invalidated, and re-push what is left.
    ///
    /// Called from the monitor refresh, which runs on every geometry change:
    /// an output that moved, shrank or went away no longer has the rectangle
    /// its shade was cut for.
    pub(crate) fn prune_monitor_locks(&mut self, backend: &mut dyn Backend) {
        if self.features.monitor_lock.is_empty() {
            return;
        }
        let present: Vec<(i32, (i32, i32, i32, i32))> = self
            .state
            .monitor_order
            .iter()
            .filter_map(|&key| self.state.monitors.get(key))
            .map(|monitor| {
                (
                    monitor.num,
                    (
                        monitor.geometry.m_x,
                        monitor.geometry.m_y,
                        monitor.geometry.m_w,
                        monitor.geometry.m_h,
                    ),
                )
            })
            .collect();
        let mut dropped: Vec<(i32, &'static str)> = self
            .features
            .monitor_lock
            .retain_outputs(&present)
            .into_iter()
            .map(|num| (num, "output_changed"))
            .collect();
        // The invariant the lock action enforces has to survive a display
        // change too: unplugging the one unlocked output would otherwise
        // leave a desktop that is shaded end to end, with nowhere to draw the
        // prompt that would lift any of it. The oldest locks give way until a
        // screen is clear again.
        while !self.features.monitor_lock.is_empty()
            && self.features.monitor_lock.len() >= present.len()
            && let Some(num) = self.features.monitor_lock.drop_oldest()
        {
            dropped.push((num, "last_unlocked_output"));
        }
        for (num, reason) in dropped {
            log::info!("Monitor {num} lock dropped: {reason}");
            self.broadcast_ipc_event(
                "monitor/lock",
                serde_json::json!({ "monitor": num, "locked": false, "reason": reason }),
            );
            // The prompt asking for a monitor that is no longer locked has
            // nothing left to unlock.
            if self.features.system_ui.monitor_lock_target() == Some(num) {
                self.close_system_ui(backend);
            }
        }
        // A geometry change re-picks the selection from the pointer, which
        // can land it on a monitor whose lock survived. Move it off, exactly
        // as locking that monitor would have.
        if self
            .state
            .sel_mon
            .is_some_and(|key| self.monitor_key_is_locked(key))
            && let Some(next) = self.first_unlocked_monitor()
            && let Err(error) = self.switch_to_monitor(backend, next)
        {
            log::warn!("Could not move the selection off a locked monitor: {error}");
        }
        self.sync_monitor_shades(backend);
        // A lock a display change took off is still a lock coming off for
        // good; the last one returns the compositor lease as the unlock does.
        self.release_lease_after_last_lock(backend);
    }
}

/// A backend for policy tests around monitor locks, shared with the idle
/// policy's tests: the dummy ops, a compositor that really switches on and
/// off, and a record of the mode and shade pushes a lock should cause.
#[cfg(test)]
pub(super) mod test_support {
    use crate::backend::api::{
        Backend, BackendDiagnostics, Capabilities, ColorAllocator, CompositorAnnotation,
        CompositorBenchmark, CompositorControl, CompositorMedia, CompositorWindowEffects,
        CompositorWorkspaceEffects, CursorProvider, DisplayControl, EventHandler, InputOps, KeyOps,
        MonitorShade, OutputOps, PropertyOps, RenderScheduler, WindowOps,
    };
    use crate::backend::common_define::{OutputId, WindowId};
    use crate::backend::error::BackendError;
    use crate::backend::wayland_dummy_ops::{
        DummyColorAllocator, DummyCursorProvider, DummyInputOps, DummyKeyOps, DummyOutputOps,
        DummyPropertyOps, DummyWindowOps,
    };
    use crate::jwm::Jwm;

    pub(crate) struct LockSpyBackend {
        window_ops: DummyWindowOps,
        input_ops: DummyInputOps,
        property_ops: DummyPropertyOps,
        output_ops: DummyOutputOps,
        key_ops: DummyKeyOps,
        cursor_provider: DummyCursorProvider,
        color_allocator: DummyColorAllocator,
        /// Whether the compositor is running; flipped by
        /// `set_compositor_enabled` the way a real backend flips it.
        pub(crate) compositor_enabled: bool,
        /// Every overview on/off push, in order.
        pub(crate) overview_modes: Vec<bool>,
        /// Every expose on/off push, in order.
        pub(crate) expose_modes: Vec<bool>,
        /// Every lock-shade payload pushed, newest last.
        pub(crate) shade_pushes: Vec<Vec<MonitorShade>>,
    }

    impl LockSpyBackend {
        pub(crate) fn new() -> Self {
            Self {
                window_ops: DummyWindowOps,
                input_ops: DummyInputOps,
                property_ops: DummyPropertyOps,
                output_ops: DummyOutputOps,
                key_ops: DummyKeyOps,
                cursor_provider: DummyCursorProvider,
                color_allocator: DummyColorAllocator,
                compositor_enabled: true,
                overview_modes: Vec::new(),
                expose_modes: Vec::new(),
                shade_pushes: Vec::new(),
            }
        }
    }

    impl CompositorBenchmark for LockSpyBackend {}
    impl BackendDiagnostics for LockSpyBackend {}
    impl CompositorControl for LockSpyBackend {}
    impl CompositorMedia for LockSpyBackend {}
    impl CompositorWorkspaceEffects for LockSpyBackend {
        fn compositor_set_overview_mode(
            &mut self,
            active: bool,
            _windows: &[(WindowId, f32, f32, f32, f32, bool, String)],
        ) {
            self.overview_modes.push(active);
        }

        fn compositor_set_monitor_shades(&mut self, shades: &[MonitorShade]) {
            self.shade_pushes.push(shades.to_vec());
        }

        fn compositor_set_expose_mode(
            &mut self,
            active: bool,
            _windows: Vec<(WindowId, i32, i32, u32, u32, String)>,
        ) {
            self.expose_modes.push(active);
        }
    }
    impl CompositorWindowEffects for LockSpyBackend {}
    impl CompositorAnnotation for LockSpyBackend {}
    impl DisplayControl for LockSpyBackend {}
    impl RenderScheduler for LockSpyBackend {
        fn has_compositor(&self) -> bool {
            self.compositor_enabled
        }
    }

    impl Backend for LockSpyBackend {
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

        fn set_compositor_enabled(&mut self, enabled: bool) -> Result<bool, BackendError> {
            if self.compositor_enabled == enabled {
                return Ok(false);
            }
            self.compositor_enabled = enabled;
            Ok(true)
        }
    }

    /// A JWM on two side-by-side 1920x1080 outputs, numbered 0 and 1, with
    /// the selection on 0.
    pub(crate) fn jwm_on_two_monitors(backend: &mut LockSpyBackend) -> Jwm {
        let mut jwm = Jwm::new_with_runtime_backend(backend, "test").expect("test jwm");
        // The dummy output is monitor 0; the second is the same panel beside it.
        let mut right = backend
            .output_ops()
            .enumerate_outputs()
            .into_iter()
            .next()
            .expect("the dummy backend has an output");
        right.id = OutputId(1);
        right.x = 1920;
        jwm.add_monitor(right);
        assert_eq!(jwm.state.monitor_order.len(), 2);
        jwm
    }
}

#[cfg(test)]
mod tests {
    use super::MonitorLockState;
    use super::test_support::{LockSpyBackend, jwm_on_two_monitors};
    use crate::backend::common_define::WindowId;
    use crate::core::models::WMClient;
    use crate::jwm::types::WMArgEnum;

    fn rect(x: i32) -> (i32, i32, i32, i32) {
        (x, 0, 1920, 1080)
    }

    #[test]
    fn locking_twice_keeps_the_first_entry_and_the_lock_order() {
        let mut state = MonitorLockState::default();
        assert!(state.lock(0, rect(0)));
        assert!(state.lock(1, rect(1920)));
        assert!(!state.lock(0, rect(0)));

        assert_eq!(state.len(), 2);
        assert_eq!(state.nums().collect::<Vec<_>>(), vec![0, 1]);
        // An unlock with no argument means the most recent lock, so the
        // order is load-bearing, not incidental.
        assert_eq!(state.latest(), Some(1));
    }

    #[test]
    fn unlocking_reports_whether_anything_came_off() {
        let mut state = MonitorLockState::default();
        state.lock(1, rect(1920));
        assert!(state.unlock(1));
        assert!(!state.unlock(1));
        assert!(state.is_empty());
        assert_eq!(state.latest(), None);
    }

    #[test]
    fn a_shade_carries_the_rectangle_the_monitor_had_when_it_was_locked() {
        let mut state = MonitorLockState::default();
        state.lock(1, (1920, -180, 2560, 1440));
        let shades = state.shades();
        assert_eq!(shades.len(), 1);
        assert_eq!(shades[0].num, 1);
        assert_eq!(
            (shades[0].x, shades[0].y, shades[0].width, shades[0].height),
            (1920, -180, 2560, 1440)
        );
    }

    /// Monitor numbers are positional. A lock that survived a display change
    /// on its number alone would shade whichever screen inherited it — and
    /// leave the one the user locked on show.
    #[test]
    fn a_lock_is_dropped_when_its_output_moves_resizes_or_goes_away() {
        let mut state = MonitorLockState::default();
        state.lock(0, rect(0));
        state.lock(1, rect(1920));
        state.lock(2, rect(3840));

        let dropped = state.retain_outputs(&[
            (0, rect(0)),
            // Number 1 is still here but somewhere else now.
            (1, (1920, 0, 1280, 720)),
            // Number 2 is gone.
        ]);

        assert_eq!(dropped, vec![1, 2]);
        assert_eq!(state.nums().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn an_unchanged_layout_keeps_every_lock() {
        let mut state = MonitorLockState::default();
        state.lock(0, rect(0));
        state.lock(1, rect(1920));

        assert!(
            state
                .retain_outputs(&[(0, rect(0)), (1, rect(1920)), (2, rect(3840))])
                .is_empty()
        );
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn the_oldest_lock_is_the_one_that_gives_way() {
        let mut state = MonitorLockState::default();
        state.lock(2, rect(3840));
        state.lock(0, rect(0));

        assert_eq!(state.drop_oldest(), Some(2));
        assert_eq!(state.nums().collect::<Vec<_>>(), vec![0]);
        assert_eq!(state.drop_oldest(), Some(0));
        assert_eq!(state.drop_oldest(), None);
    }

    #[test]
    fn clearing_reports_every_lock_it_took_off() {
        let mut state = MonitorLockState::default();
        state.lock(0, rect(0));
        state.lock(1, rect(1920));
        assert_eq!(state.clear(), vec![0, 1]);
        assert!(state.is_empty());
    }

    /// A window on `monitor`'s first tag, known to the WM but not mapped by
    /// any backend: enough for the lock's "which windows are on it" tests.
    fn client_on(
        jwm: &mut crate::jwm::Jwm,
        raw: u64,
        monitor: usize,
    ) -> crate::core::models::ClientKey {
        let mut client = WMClient::new(WindowId::from_raw(raw));
        client.mon = Some(jwm.state.monitor_order[monitor]);
        client.state.tags = 1;
        jwm.insert_client(client)
    }

    /// Expose was entered while every monitor was unlocked, so its grid holds
    /// the windows of the monitor now going dark, spread across the desktop
    /// where the shade does not reach. Locking takes expose down with it.
    #[test]
    fn a_lock_takes_expose_down_rather_than_leave_the_windows_on_show() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        client_on(&mut jwm, 0x701, 0);
        jwm.features.expose_active = true;

        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(0))
            .expect("monitor 0 locks");

        assert!(jwm.monitor_is_locked(0));
        assert!(!jwm.features.expose_active, "expose is over");
        assert_eq!(backend.expose_modes, vec![false], "and the grid is gone");
        assert_eq!(
            backend.shade_pushes.last().map(Vec::len),
            Some(1),
            "the shade still goes up"
        );
    }

    /// The overview's prism sits on the monitor it was entered on. Locked
    /// there, it would be drawn under the shade while still swallowing every
    /// key; it comes down. An overview on another monitor shows nothing of the
    /// locked one and is left alone.
    #[test]
    fn a_lock_takes_down_an_overview_of_that_monitor_and_only_that_one() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        let on_left = client_on(&mut jwm, 0x711, 0);
        jwm.features.overview.activate(vec![on_left], Some(0));

        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(0))
            .expect("monitor 0 locks");

        assert!(
            !jwm.features.overview.active,
            "the prism is not left under the shade"
        );
        assert_eq!(backend.overview_modes, vec![false]);

        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        let on_right = client_on(&mut jwm, 0x712, 1);
        jwm.features.overview.activate(vec![on_right], Some(0));

        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(0))
            .expect("monitor 0 locks");

        assert!(jwm.features.overview.active, "an overview elsewhere stays");
        assert!(backend.overview_modes.is_empty());
    }

    /// A panel opened on a session running without a compositor leases one;
    /// a monitor locked from it keeps the lease past the panel's close,
    /// because the shade needs the renderer. When a display change then takes
    /// the last lock off, that lease goes back exactly as the unlock would
    /// have returned it.
    #[test]
    fn a_display_change_that_drops_the_last_lock_returns_the_leased_compositor() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.system_ui_temporary_compositor = true;
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");

        // Monitor 1 changes mode: its shade no longer fits it.
        let right = jwm.state.monitor_order[1];
        jwm.state.monitors[right].geometry.m_w = 1280;
        jwm.prune_monitor_locks(&mut backend);

        assert!(!jwm.monitor_is_locked(1));
        assert!(
            !backend.compositor_enabled,
            "the leased compositor is off again"
        );
        assert!(!jwm.features.system_ui_temporary_compositor);
    }

    /// The lease is not pulled from under a panel that is still on screen: it
    /// may be drawing on the very same lease. Its own close returns it, now
    /// that nothing is locked.
    #[test]
    fn a_dropped_last_lock_leaves_the_lease_to_a_panel_still_on_screen() {
        let mut backend = LockSpyBackend::new();
        let mut jwm = jwm_on_two_monitors(&mut backend);
        jwm.features.system_ui_temporary_compositor = true;
        jwm.lock_monitor(&mut backend, &WMArgEnum::Int(1))
            .expect("monitor 1 locks");
        jwm.features.system_ui = crate::jwm::features::SystemUiState::wifi_picker("");

        let right = jwm.state.monitor_order[1];
        jwm.state.monitors[right].geometry.m_w = 1280;
        jwm.prune_monitor_locks(&mut backend);

        assert!(!jwm.monitor_is_locked(1));
        assert!(backend.compositor_enabled, "the panel still draws on it");
        assert!(jwm.features.system_ui_temporary_compositor);

        jwm.close_system_ui(&mut backend);
        assert!(!backend.compositor_enabled);
        assert!(!jwm.features.system_ui_temporary_compositor);
    }
}
