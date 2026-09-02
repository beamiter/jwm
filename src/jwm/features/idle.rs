//! What to do when nobody is at the machine.
//!
//! DMS and Noctalia both ship an idle daemon; JWM had the pieces — a PAM lock
//! screen, a compositor brightness knob — but nothing deciding when to use
//! them. This module is that decision, and only the decision: it turns "the
//! session has been idle for N seconds" into a list of actions, and the caller
//! performs them. Reading the idle clock is the backend's job and running a
//! command is the toggle's; keeping both out of here is what makes every rule
//! below a unit test rather than a wait-five-minutes-and-see.
//!
//! The rules that are easy to get wrong, and are therefore pinned down here:
//! an action fires once per idle episode and not once per frame; activity
//! undims but never unlocks, because dismissing the lock screen is the
//! password's job; an inhibitor wakes the session back up rather than merely
//! freezing it dimmed; and each stage is judged against its own timeout, so a
//! configuration whose stages are out of order still behaves sensibly.
//!
//! Two of those rules exist because a lock timeout is the one stage that can
//! take the session away from the person using it. A timeout shorter than
//! [`MIN_LOCK_SECS`] is raised to it, because `idle_lock_secs = 1` locks
//! faster than a password can be typed and leaves no way back in except
//! editing the config blind; and a lock that has just been dismissed does not
//! re-arm for [`UNLOCK_GRACE`], so unlocking always buys enough time to work
//! or to change the setting.

use std::time::{Duration, Instant};

/// Brightness the dim stage falls back to when the configured level is
/// unusable.
pub const DEFAULT_DIM_LEVEL: f32 = 0.35;

/// The shortest lock timeout that leaves a session usable. Anything smaller
/// and non-zero is raised to this: a one-second lock re-locks between
/// keystrokes of the password, which is not a stricter policy but a session
/// nobody can get back into. Zero still switches the stage off outright.
pub const MIN_LOCK_SECS: u64 = 30;

/// How long after an unlock the lock stage stays disarmed. Typing the
/// password is a statement that somebody is here; taking the screen back a
/// second later calls them a liar.
pub const UNLOCK_GRACE: Duration = Duration::from_secs(60);

/// How long to wait before asking for a lock again after one failed. Locking
/// fails when something else holds the pointer or keyboard — a menu, a drag —
/// which is a passing condition, so the session must not be left unlocked for
/// the rest of the idle period because of it.
pub const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// One thing the idle policy wants done.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IdleAction {
    /// Fade the screen to this fraction of normal brightness.
    Dim(f32),
    /// Put the brightness back.
    Undim,
    /// Show the lock screen.
    Lock,
    /// Run the configured screen-off command.
    ScreenOff,
    /// Run the configured screen-on command, because a screen-off ran.
    ScreenOn,
}

/// When each stage fires. `None` means the stage is switched off.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct IdleSettings {
    pub dim_after: Option<Duration>,
    pub dim_level: f32,
    pub lock_after: Option<Duration>,
    pub screen_off_after: Option<Duration>,
}

impl IdleSettings {
    /// Build from the configured seconds. Zero switches a stage off, the
    /// screen-off stage additionally needs a command to run, and a non-zero
    /// lock timeout is raised to [`MIN_LOCK_SECS`].
    #[must_use]
    pub fn from_secs(
        dim_secs: u64,
        dim_level: f32,
        lock_secs: u64,
        screen_off_secs: u64,
        has_screen_off_command: bool,
    ) -> Self {
        let stage = |secs: u64| (secs > 0).then(|| Duration::from_secs(secs));
        Self {
            dim_after: stage(dim_secs),
            dim_level: if (0.0..=1.0).contains(&dim_level) {
                dim_level
            } else {
                DEFAULT_DIM_LEVEL
            },
            // Clamped rather than rejected: a too-eager timeout still means
            // "lock this session", and honouring that at the shortest usable
            // interval is closer to the intent than switching it off.
            lock_after: stage(lock_secs).map(|_| Duration::from_secs(lock_secs.max(MIN_LOCK_SECS))),
            screen_off_after: has_screen_off_command
                .then(|| stage(screen_off_secs))
                .flatten(),
        }
    }

    /// Whether anything is configured at all. Nothing configured means the
    /// idle clock does not even need to be read.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.dim_after.is_some() || self.lock_after.is_some() || self.screen_off_after.is_some()
    }

    /// The earliest stage, which is also the point below which the session
    /// counts as awake.
    #[must_use]
    pub fn first_stage(&self) -> Option<Duration> {
        [self.dim_after, self.lock_after, self.screen_off_after]
            .into_iter()
            .flatten()
            .min()
    }
}

/// What the idle policy has already done, so it does not do it every frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleTracker {
    dimmed: bool,
    screen_off: bool,
    lock_asked: bool,
    /// The lock state at the previous poll, so an unlock can be noticed
    /// without the lock screen having to announce one.
    was_locked: bool,
    unlocked_at: Option<Instant>,
    lock_retry_at: Option<Instant>,
    lock_failures: u32,
}

impl IdleTracker {
    /// The actions to perform now, given how long the session has been idle.
    ///
    /// `inhibited` covers everything that should hold the session awake — the
    /// caffeine toggle, a client's idle inhibitor, a recording in progress.
    /// `locked` is whether the lock screen is already up, and `now` is only
    /// used for the two lock timers, so every other rule stays a matter of
    /// how long the session has been idle.
    pub fn poll(
        &mut self,
        settings: &IdleSettings,
        idle: Duration,
        inhibited: bool,
        locked: bool,
        now: Instant,
    ) -> Vec<IdleAction> {
        // Noticed here rather than reported by the lock screen: the password
        // being accepted and the lock coming down are the same event as far
        // as this policy is concerned, and one of them is already an input.
        if self.was_locked && !locked {
            self.unlocked_at = Some(now);
        }
        self.was_locked = locked;
        if locked {
            self.lock_retry_at = None;
            self.lock_failures = 0;
        }

        let awake = inhibited
            || settings
                .first_stage()
                .is_none_or(|first_stage| idle < first_stage);
        if awake {
            return self.wake();
        }

        let mut actions = Vec::new();
        if let Some(after) = settings.dim_after
            && idle >= after
            && !self.dimmed
        {
            self.dimmed = true;
            actions.push(IdleAction::Dim(settings.dim_level));
        }
        // Asked at most once per idle period, and never when the lock screen
        // is already up. The attempt is remembered separately from the result
        // because locking can fail — something else may hold the pointer, and
        // a session with no compositor has no lock screen to show — so a
        // failure is retried on a timer by `note_lock_failed` rather than
        // every frame, which would fill the log rather than the screen.
        if let Some(after) = settings.lock_after
            && idle >= after
            && !locked
            && !self.lock_asked
            && !self.in_unlock_grace(now)
            && self.lock_retry_at.is_none_or(|retry_at| now >= retry_at)
        {
            self.lock_asked = true;
            self.lock_retry_at = None;
            actions.push(IdleAction::Lock);
        }
        if let Some(after) = settings.screen_off_after
            && idle >= after
            && !self.screen_off
        {
            self.screen_off = true;
            actions.push(IdleAction::ScreenOff);
        }
        actions
    }

    /// Whether an unlock is recent enough that the lock stage stays disarmed.
    fn in_unlock_grace(&self, now: Instant) -> bool {
        self.unlocked_at
            .is_some_and(|at| now.saturating_duration_since(at) < UNLOCK_GRACE)
    }

    /// The lock attempt did not take. Arms a retry instead of leaving the
    /// session unlocked for the rest of the idle period, and reports how many
    /// times in a row it has failed so the caller can log the first loudly
    /// and the rest quietly.
    pub fn note_lock_failed(&mut self, now: Instant) -> u32 {
        self.lock_asked = false;
        self.lock_retry_at = Some(now + LOCK_RETRY_INTERVAL);
        self.lock_failures = self.lock_failures.saturating_add(1);
        self.lock_failures
    }

    /// Undo what is undoable. Never emits `Lock`'s opposite: only the password
    /// dismisses the lock screen.
    fn wake(&mut self) -> Vec<IdleAction> {
        let mut actions = Vec::new();
        self.lock_asked = false;
        self.lock_retry_at = None;
        self.lock_failures = 0;
        if self.screen_off {
            self.screen_off = false;
            actions.push(IdleAction::ScreenOn);
        }
        if self.dimmed {
            self.dimmed = false;
            actions.push(IdleAction::Undim);
        }
        actions
    }

    /// Whether the screen is currently dimmed by the idle policy.
    #[must_use]
    pub fn is_dimmed(&self) -> bool {
        self.dimmed
    }

    /// Whether the screen-off command has run and not yet been undone.
    #[must_use]
    pub fn is_screen_off(&self) -> bool {
        self.screen_off
    }
}

/// How often the idle clock is read. The session's event loop already wakes
/// about this often, so nothing is kept awake to ask.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn configured_idle_settings() -> IdleSettings {
    let cfg = crate::config::CONFIG.load();
    let behavior = cfg.behavior();
    warn_about_a_short_lock_timeout(behavior.idle_lock_secs);
    IdleSettings::from_secs(
        behavior.idle_dim_secs,
        behavior.idle_dim_level,
        behavior.idle_lock_secs,
        behavior.idle_screen_off_secs,
        !behavior.idle_screen_off_command.trim().is_empty(),
    )
}

/// Say once, per value, that a lock timeout was raised to the floor. Said
/// here because this is the one place the configured seconds are read, and
/// said at most once because the read happens every second.
fn warn_about_a_short_lock_timeout(lock_secs: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    // `u64::MAX` is the "nothing said yet" mark; no configuration reaches it.
    static WARNED_FOR: AtomicU64 = AtomicU64::new(u64::MAX);
    if WARNED_FOR.swap(lock_secs, Ordering::Relaxed) == lock_secs {
        return;
    }
    if (1..MIN_LOCK_SECS).contains(&lock_secs) {
        log::warn!(
            "Idle: behavior.idle_lock_secs={lock_secs} is shorter than the {MIN_LOCK_SECS}s \
             floor and would re-lock faster than a password can be typed; locking after \
             {MIN_LOCK_SECS}s instead. Set it to 0 to switch idle locking off."
        );
    }
}

fn idle_poll_wakeup(
    enabled: bool,
    restore_pending: bool,
    last_poll: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<Duration> {
    if !enabled {
        return restore_pending.then_some(Duration::ZERO);
    }
    Some(last_poll.map_or(Duration::ZERO, |last| {
        POLL_INTERVAL.saturating_sub(now.saturating_duration_since(last))
    }))
}

impl crate::jwm::Jwm {
    pub(crate) fn idle_next_wakeup(&self, now: std::time::Instant) -> Option<Duration> {
        let settings = configured_idle_settings();
        idle_poll_wakeup(
            settings.is_enabled(),
            self.idle.is_dimmed() || self.idle.is_screen_off(),
            self.last_idle_poll,
            now,
        )
    }

    /// Read the idle clock and carry out what the policy asks for. Called
    /// from the maintenance update; does nothing until the interval is up.
    pub(crate) fn poll_idle(&mut self, backend: &mut dyn crate::backend::api::Backend) {
        let settings = configured_idle_settings();
        if !settings.is_enabled() {
            // Switched off while it had already dimmed the screen: put the
            // screen back rather than leaving the session dark.
            for action in self
                .idle
                .poll(&settings, Duration::ZERO, true, false, Instant::now())
            {
                self.apply_idle_action(backend, action);
            }
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_idle_poll
            .is_some_and(|last| now.saturating_duration_since(last) < POLL_INTERVAL)
        {
            return;
        }
        self.last_idle_poll = Some(now);

        // Two idle policies in one session do not share the work, they fight:
        // the X server's blanker resets the very clock read below, so a stage
        // later than the server's own timeout would never be reached. Once
        // this session has a policy, it is the only one.
        if !self.server_saver_suppressed {
            self.server_saver_suppressed = true;
            if backend.suppress_server_screensaver() {
                log::info!("Idle: the display server's own blanking is now off");
            }
        }

        // No idle clock: this backend cannot tell activity from absence, and
        // guessing would dim the screen of somebody who is working.
        let Some(idle_millis) = backend.idle_millis() else {
            return;
        };
        let inhibited = self.idle_inhibited
            || backend.idle_inhibited_by_client()
            // Recording an unattended screen is exactly when the machine
            // looks idle and must not be.
            || self.features.recording.active
            || self.features.audio_recording.active;
        let actions = self.idle.poll(
            &settings,
            Duration::from_millis(idle_millis),
            inhibited,
            self.features.system_ui.is_locked(),
            now,
        );
        for action in actions {
            self.apply_idle_action(backend, action);
        }
    }

    /// Put the dim back after a config apply. Applying the configuration
    /// re-sends `behavior.brightness` to the compositor, which would otherwise
    /// brighten a dimmed screen and leave it bright until the next idle
    /// period — a screen that lights up on its own for no visible reason.
    pub(crate) fn reapply_idle_dim(&mut self, backend: &mut dyn crate::backend::api::Backend) {
        if !self.idle.is_dimmed() {
            return;
        }
        // Validated the same way the dim stage validates it, so an out-of-range
        // `idle_dim_level` cannot make a config reload land on a different
        // brightness than the dim it is restoring.
        let level = IdleSettings::from_secs(
            1,
            crate::config::CONFIG.load().behavior().idle_dim_level,
            0,
            0,
            false,
        )
        .dim_level;
        let brightness = configured_brightness() * level;
        log::info!("Idle: re-applying the dim after a config change ({brightness})");
        backend.compositor_set_brightness(brightness);
    }

    fn apply_idle_action(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
        action: IdleAction,
    ) {
        match action {
            IdleAction::Dim(level) => {
                let brightness = configured_brightness() * level;
                log::info!("Idle: dimming to {level} (brightness {brightness})");
                backend.compositor_set_brightness(brightness);
                self.broadcast_idle_state();
            }
            IdleAction::Undim => {
                // Logged as loudly as the dim: a dim with no matching restore
                // in the log is the whole symptom of a screen that stays dark,
                // and without this line there is nothing to tell the two apart.
                let brightness = configured_brightness();
                log::info!("Idle: restoring brightness to {brightness}");
                backend.compositor_set_brightness(brightness);
                self.broadcast_idle_state();
            }
            IdleAction::Lock => {
                log::info!("Idle: locking");
                if let Err(error) = self.lock_screen(backend, &crate::jwm::types::WMArgEnum::Int(0))
                {
                    // Something transient — a menu holding the pointer grab —
                    // must not leave the session unlocked until the next time
                    // somebody touches the keyboard. The first failure is
                    // worth a warning; a menu left open all night is not.
                    let failures = self.idle.note_lock_failed(std::time::Instant::now());
                    let retry = LOCK_RETRY_INTERVAL.as_secs();
                    if failures == 1 {
                        log::warn!("Idle: could not lock, retrying in {retry}s: {error}");
                    } else {
                        log::debug!(
                            "Idle: could not lock ({failures} in a row), retrying in {retry}s: {error}"
                        );
                    }
                }
            }
            IdleAction::ScreenOff => {
                let command = crate::config::CONFIG
                    .load()
                    .behavior()
                    .idle_screen_off_command
                    .clone();
                if let Some(child) = run_idle_command("screen off", &command) {
                    self.supervise_transient_child(child);
                }
                self.broadcast_idle_state();
            }
            IdleAction::ScreenOn => {
                let command = crate::config::CONFIG
                    .load()
                    .behavior()
                    .idle_screen_on_command
                    .clone();
                if !command.trim().is_empty() {
                    if let Some(child) = run_idle_command("screen on", &command) {
                        self.supervise_transient_child(child);
                    }
                }
                self.broadcast_idle_state();
            }
        }
    }

    /// Hold the session awake, or let it idle again.
    pub fn toggle_idle_inhibit(
        &mut self,
        backend: &mut dyn crate::backend::api::Backend,
        _arg: &crate::jwm::types::WMArgEnum,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.idle_inhibited = !self.idle_inhibited;
        log::info!(
            "Idle inhibit {}",
            if self.idle_inhibited { "ON" } else { "OFF" }
        );
        // Take effect now rather than at the next interval: switching it on
        // while the screen is already dim should brighten it immediately.
        self.last_idle_poll = None;
        self.poll_idle(backend);
        self.broadcast_idle_state();
        Ok(())
    }

    /// The idle policy's state, for `get_idle_status` and the `idle` topic.
    pub(crate) fn idle_status_json(&self) -> serde_json::Value {
        let cfg = crate::config::CONFIG.load();
        let behavior = cfg.behavior();
        serde_json::json!({
            "inhibited": self.idle_inhibited,
            "dimmed": self.idle.is_dimmed(),
            "screen_off": self.idle.is_screen_off(),
            "locked": self.features.system_ui.is_locked(),
            "dim_secs": behavior.idle_dim_secs,
            "lock_secs": behavior.idle_lock_secs,
            "screen_off_secs": behavior.idle_screen_off_secs,
        })
    }

    fn broadcast_idle_state(&mut self) {
        let payload = self.idle_status_json();
        self.broadcast_ipc_event("idle/state", payload);
    }
}

/// The brightness the session runs at when it is not dimmed.
fn configured_brightness() -> f32 {
    crate::config::CONFIG.load().behavior().brightness
}

fn run_idle_command(what: &str, command: &str) -> Option<std::process::Child> {
    let Some((program, args)) = crate::jwm::features::session::split_command(command) else {
        log::warn!("Idle: no {what} command configured");
        return None;
    };
    log::info!("Idle: {what} \u{2192} {command}");
    match std::process::Command::new(&program).args(&args).spawn() {
        Ok(child) => Some(child),
        Err(error) => {
            log::warn!("Idle: could not run {command:?}: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> IdleSettings {
        IdleSettings::from_secs(60, 0.3, 300, 600, true)
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// A fixed origin every lock timer in these tests is measured from, so a
    /// test that does not care about wall-clock time never accidentally
    /// depends on how long it took to run.
    fn origin() -> Instant {
        // Far enough ahead that subtracting a grace period cannot underflow.
        Instant::now() + UNLOCK_GRACE + UNLOCK_GRACE
    }

    fn now() -> Instant {
        origin()
    }

    #[test]
    fn poll_wakeup_is_exact_and_disabled_policy_settles() {
        let now = std::time::Instant::now();
        assert_eq!(idle_poll_wakeup(false, false, None, now), None);
        assert_eq!(
            idle_poll_wakeup(false, true, Some(now), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            idle_poll_wakeup(true, false, None, now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            idle_poll_wakeup(
                true,
                false,
                Some(now),
                now + POLL_INTERVAL - Duration::from_nanos(1),
            ),
            Some(Duration::from_nanos(1))
        );
        assert_eq!(
            idle_poll_wakeup(true, false, Some(now), now + POLL_INTERVAL),
            Some(Duration::ZERO)
        );
        assert_eq!(
            idle_poll_wakeup(true, false, Some(now + Duration::from_secs(2)), now,),
            Some(POLL_INTERVAL)
        );
    }

    #[test]
    fn zero_switches_a_stage_off() {
        let settings = IdleSettings::from_secs(0, 0.3, 0, 0, true);
        assert!(!settings.is_enabled());
        assert_eq!(settings.first_stage(), None);
        assert!(
            IdleTracker::default()
                .poll(&settings, secs(9999), false, false, now())
                .is_empty()
        );
    }

    #[test]
    fn the_screen_off_stage_needs_a_command_to_run() {
        let with = IdleSettings::from_secs(0, 0.3, 0, 600, true);
        let without = IdleSettings::from_secs(0, 0.3, 0, 600, false);
        assert_eq!(with.screen_off_after, Some(secs(600)));
        assert_eq!(without.screen_off_after, None);
        assert!(!without.is_enabled());
    }

    #[test]
    fn an_unusable_dim_level_falls_back() {
        assert_eq!(
            IdleSettings::from_secs(60, 1.4, 0, 0, false).dim_level,
            DEFAULT_DIM_LEVEL
        );
        assert_eq!(
            IdleSettings::from_secs(60, -0.2, 0, 0, false).dim_level,
            DEFAULT_DIM_LEVEL
        );
        assert_eq!(IdleSettings::from_secs(60, 0.0, 0, 0, false).dim_level, 0.0);
    }

    #[test]
    fn each_stage_fires_once_and_only_once() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        assert!(
            tracker
                .poll(&settings, secs(59), false, false, now())
                .is_empty()
        );
        assert_eq!(
            tracker.poll(&settings, secs(60), false, false, now()),
            [IdleAction::Dim(0.3)]
        );
        // Same stage, later frame: nothing more to do.
        assert!(
            tracker
                .poll(&settings, secs(120), false, false, now())
                .is_empty()
        );
        assert_eq!(
            tracker.poll(&settings, secs(300), false, false, now()),
            [IdleAction::Lock]
        );
        assert_eq!(
            tracker.poll(&settings, secs(600), false, true, now()),
            [IdleAction::ScreenOff]
        );
        assert!(
            tracker
                .poll(&settings, secs(900), false, true, now())
                .is_empty()
        );
    }

    #[test]
    fn a_failed_lock_is_not_retried_every_frame() {
        // `locked` never becomes true: this session has no lock screen to
        // show. One attempt is a warning in the log; one per frame is a flood.
        let settings = settings();
        let mut tracker = IdleTracker::default();
        assert_eq!(
            tracker.poll(&settings, secs(300), false, false, now()),
            [IdleAction::Dim(0.3), IdleAction::Lock]
        );
        assert!(
            !tracker
                .poll(&settings, secs(301), false, false, now())
                .contains(&IdleAction::Lock)
        );
    }

    #[test]
    fn a_lock_repeats_only_after_an_unlock() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();
        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start)
                .contains(&IdleAction::Lock)
        );
        // Still locked: the window manager already has the screen.
        assert!(
            !tracker
                .poll(&settings, secs(400), false, true, start)
                .contains(&IdleAction::Lock)
        );
        // Typing the password is activity, and the idle period after the
        // grace it buys locks again.
        tracker.poll(&settings, secs(0), false, false, start);
        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start + UNLOCK_GRACE)
                .contains(&IdleAction::Lock)
        );
    }

    #[test]
    fn activity_undoes_everything_except_the_lock() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        tracker.poll(&settings, secs(600), false, false, now());
        assert!(tracker.is_dimmed() && tracker.is_screen_off());

        // A keystroke: the screen comes back, the lock screen stays up for the
        // password to dismiss.
        let woken = tracker.poll(&settings, secs(0), false, true, now());
        assert_eq!(woken, [IdleAction::ScreenOn, IdleAction::Undim]);
        assert!(!tracker.is_dimmed() && !tracker.is_screen_off());
        // Idle again: the stages arm again.
        assert_eq!(
            tracker.poll(&settings, secs(60), false, true, now()),
            [IdleAction::Dim(0.3)]
        );
    }

    #[test]
    fn an_inhibitor_wakes_the_session_rather_than_freezing_it() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        tracker.poll(&settings, secs(60), false, false, now());
        assert!(tracker.is_dimmed());

        // A video starts. Being idle no longer counts, and the dim it already
        // caused is undone rather than left on screen for the whole film.
        assert_eq!(
            tracker.poll(&settings, secs(120), true, false, now()),
            [IdleAction::Undim]
        );
        assert!(
            tracker
                .poll(&settings, secs(9999), true, false, now())
                .is_empty()
        );
        // The video ends; the policy takes over again.
        assert_eq!(
            tracker.poll(&settings, secs(9999), false, false, now()),
            [
                IdleAction::Dim(0.3),
                IdleAction::Lock,
                IdleAction::ScreenOff
            ]
        );
    }

    #[test]
    fn a_dim_still_happens_while_locked() {
        let settings = IdleSettings::from_secs(60, 0.3, 0, 0, false);
        let mut tracker = IdleTracker::default();
        assert_eq!(
            tracker.poll(&settings, secs(60), false, true, now()),
            [IdleAction::Dim(0.3)]
        );
    }

    #[test]
    fn stages_configured_out_of_order_each_keep_their_own_timeout() {
        // Screen off before lock: unusual, but every stage is judged on its
        // own timeout, so nothing is skipped or reordered into nonsense.
        let settings = IdleSettings::from_secs(0, 0.3, 600, 120, true);
        let mut tracker = IdleTracker::default();
        assert_eq!(
            tracker.poll(&settings, secs(120), false, false, now()),
            [IdleAction::ScreenOff]
        );
        assert_eq!(
            tracker.poll(&settings, secs(600), false, false, now()),
            [IdleAction::Lock]
        );
    }

    #[test]
    fn waking_from_a_stage_that_never_ran_does_nothing() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        assert!(
            tracker
                .poll(&settings, secs(0), false, false, now())
                .is_empty()
        );
        assert!(
            tracker
                .poll(&settings, secs(0), true, false, now())
                .is_empty()
        );
    }

    #[test]
    fn a_lock_timeout_below_the_floor_is_raised_to_it() {
        // `idle_lock_secs = 1` re-locks between the keystrokes of the
        // password. Honoured as the shortest usable timeout instead.
        let settings = IdleSettings::from_secs(0, 0.3, 1, 0, false);
        assert_eq!(settings.lock_after, Some(secs(MIN_LOCK_SECS)));
        assert!(settings.is_enabled());
        assert_eq!(settings.first_stage(), Some(secs(MIN_LOCK_SECS)));

        let mut tracker = IdleTracker::default();
        assert!(
            tracker
                .poll(&settings, secs(1), false, false, now())
                .is_empty()
        );
        assert_eq!(
            tracker.poll(&settings, secs(MIN_LOCK_SECS), false, false, now()),
            [IdleAction::Lock]
        );

        // Zero still means off, and a timeout at or above the floor is left
        // exactly as configured.
        assert_eq!(
            IdleSettings::from_secs(0, 0.3, 0, 0, false).lock_after,
            None
        );
        assert_eq!(
            IdleSettings::from_secs(0, 0.3, MIN_LOCK_SECS, 0, false).lock_after,
            Some(secs(MIN_LOCK_SECS))
        );
        assert_eq!(
            IdleSettings::from_secs(0, 0.3, 600, 0, false).lock_after,
            Some(secs(600))
        );
    }

    #[test]
    fn an_unlock_buys_a_grace_period_before_the_next_lock() {
        // The grace only bites when the lock timeout is shorter than it, so
        // this is the clamped `idle_lock_secs = 1` case: locking 30s after
        // every unlock would still be a session nobody can work in.
        let settings = IdleSettings::from_secs(5, 0.3, 1, 0, false);
        assert_eq!(settings.lock_after, Some(secs(MIN_LOCK_SECS)));
        let mut tracker = IdleTracker::default();
        let start = origin();

        assert_eq!(
            tracker.poll(&settings, secs(30), false, false, start),
            [IdleAction::Dim(0.3), IdleAction::Lock]
        );
        // The lock screen is up, and then the password dismisses it.
        tracker.poll(&settings, secs(40), false, true, start + secs(10));
        assert_eq!(
            tracker.poll(&settings, secs(0), false, false, start + secs(20)),
            [IdleAction::Undim]
        );

        // Idle for the full timeout again, half a minute after the password
        // was typed: taking the screen back now would only stop the person
        // who just proved they are here.
        let during_grace = tracker.poll(&settings, secs(30), false, false, start + secs(50));
        assert!(!during_grace.contains(&IdleAction::Lock));
        // Dimming is not the lock screen and is not held back by the grace.
        assert_eq!(during_grace, [IdleAction::Dim(0.3)]);

        // Once the grace is up, the policy locks again as usual.
        assert!(
            tracker
                .poll(
                    &settings,
                    secs(45),
                    false,
                    false,
                    start + secs(20) + UNLOCK_GRACE
                )
                .contains(&IdleAction::Lock)
        );
    }

    #[test]
    fn the_grace_period_starts_at_the_unlock_not_at_the_lock() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();

        tracker.poll(&settings, secs(300), false, false, start);
        // A long night at the lock screen: the grace must not quietly expire
        // while it is still up, or the unlock buys nothing.
        tracker.poll(&settings, secs(9999), false, true, start + secs(9999));
        tracker.poll(&settings, secs(0), false, false, start + secs(9999));
        assert!(
            !tracker
                .poll(&settings, secs(300), false, false, start + secs(10_000))
                .contains(&IdleAction::Lock)
        );
    }

    #[test]
    fn a_failed_lock_is_retried_on_a_timer() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();

        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start)
                .contains(&IdleAction::Lock)
        );
        // A menu held the pointer grab, so nothing was locked.
        assert_eq!(tracker.note_lock_failed(start), 1);
        // Not retried every frame...
        assert!(
            !tracker
                .poll(&settings, secs(301), false, false, start + secs(1))
                .contains(&IdleAction::Lock)
        );
        // ...but retried, rather than the session being left unlocked until
        // somebody touches the keyboard again.
        assert!(
            tracker
                .poll(
                    &settings,
                    secs(305),
                    false,
                    false,
                    start + LOCK_RETRY_INTERVAL
                )
                .contains(&IdleAction::Lock)
        );
        assert_eq!(tracker.note_lock_failed(start + LOCK_RETRY_INTERVAL), 2);
        // A lock that finally lands clears the streak, and activity does too.
        tracker.poll(&settings, secs(400), false, true, start + secs(20));
        assert_eq!(tracker.note_lock_failed(start + secs(20)), 1);
    }

    #[test]
    fn a_retry_is_dropped_the_moment_the_session_wakes() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();

        tracker.poll(&settings, secs(300), false, false, start);
        tracker.note_lock_failed(start);
        // Somebody came back: the pending retry is about an idle period that
        // is over, and the next one starts its own timing.
        tracker.poll(&settings, secs(0), false, false, start + secs(1));
        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start + secs(2))
                .contains(&IdleAction::Lock)
        );
    }
}
