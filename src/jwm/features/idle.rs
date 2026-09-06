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
/// nobody can get back into. A minute matches [`UNLOCK_GRACE`], so the
/// shortest timeout anyone can configure is also the shortest interval an
/// unlock buys back. Zero still switches the stage off outright.
pub const MIN_LOCK_SECS: u64 = 60;

/// How long after an unlock the lock stage stays disarmed. Typing the
/// password is a statement that somebody is here; taking the screen back a
/// second later calls them a liar.
pub const UNLOCK_GRACE: Duration = Duration::from_secs(60);

/// How long to wait before asking for a lock again after one failed. Locking
/// fails when something else holds the pointer or keyboard — a menu, a drag —
/// which is a passing condition, so the session must not be left unlocked for
/// the rest of the idle period because of it.
pub const LOCK_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// How many times in a row a refusal whose cause the backend did not explain
/// is retried before it is treated as settled for this idle period. At
/// [`LOCK_RETRY_INTERVAL`] apart this is a minute of asking, which outlasts a
/// VT switch or a compositor restart, while costing a backend that genuinely
/// cannot start one a bounded handful of attempts rather than one every five
/// seconds until morning.
pub const UNEXPLAINED_LOCK_RETRIES: u32 = 12;

/// Why a lock attempt failed, as far as retrying is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockFailure {
    /// Something that passes on its own — a menu holding the pointer grab,
    /// another panel open. Worth asking again in a moment.
    Transient,
    /// The backend tried to start a compositor for the lock screen and the
    /// attempt came back with an error whose cause travels as prose this
    /// module cannot read. A VT switch, a DRM master handed to somebody
    /// else, a momentary GLX failure all land here and all clear on their
    /// own, so this is retried — but only [`UNEXPLAINED_LOCK_RETRIES`]
    /// times, because a backend with no working renderer at all would
    /// otherwise be asked all night.
    Unexplained,
    /// Nothing this session can change: the backend reported the compositor
    /// reconciled and there is still none to draw the lock screen on. That
    /// is a statement about this backend's capability rather than about this
    /// moment, so the next attempt waits for the next idle period.
    Permanent,
}

/// Whether a lock refusal is worth asking about again inside this idle
/// period. `failures_in_a_row` counts this failure too, so the first call
/// after a refusal passes `1`.
///
/// The whole point of retrying at all is that an unattended session which
/// stops asking stays unlocked until somebody touches the keyboard — so only
/// a refusal that names a capability the backend does not have gives up at
/// once, and a refusal whose cause is unknown gets a bounded budget rather
/// than the benefit of neither doubt.
#[must_use]
pub const fn lock_failure_retries(failure: LockFailure, failures_in_a_row: u32) -> bool {
    match failure {
        LockFailure::Transient => true,
        LockFailure::Unexplained => failures_in_a_row < UNEXPLAINED_LOCK_RETRIES,
        LockFailure::Permanent => false,
    }
}

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
    /// The backend answered the last clock read with nothing. There is then
    /// nothing to poll for, and the wakeup timer stands down until a read
    /// succeeds again.
    clock_unavailable: bool,
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

    /// The lock attempt did not take. A failure still worth retrying arms a
    /// retry instead of leaving the session unlocked for the rest of the idle
    /// period; one that is not is over for this idle period, and `wake`
    /// re-arms it for the next. Reports how many times in a row it has failed
    /// so the caller can log the first loudly and the rest quietly, and so it
    /// can re-derive the same decision through [`lock_failure_retries`].
    pub fn note_lock_failed(&mut self, now: Instant, failure: LockFailure) -> u32 {
        self.lock_failures = self.lock_failures.saturating_add(1);
        if lock_failure_retries(failure, self.lock_failures) {
            self.lock_asked = false;
            self.lock_retry_at = Some(now + LOCK_RETRY_INTERVAL);
        } else {
            self.lock_asked = true;
            self.lock_retry_at = None;
        }
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

    /// Record whether the backend could read its idle clock this poll.
    pub fn note_clock(&mut self, available: bool) {
        self.clock_unavailable = !available;
    }

    /// Whether the last clock read came back empty.
    #[must_use]
    pub fn clock_unavailable(&self) -> bool {
        self.clock_unavailable
    }
}

/// How often the idle clock is read. The session's event loop already wakes
/// about this often, so nothing is kept awake to ask.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn configured_idle_settings() -> IdleSettings {
    let cfg = crate::config::CONFIG.load();
    let behavior = cfg.behavior();
    warn_about_a_short_lock_timeout(behavior.idle_lock_secs);
    warn_about_an_unusable_dim_level(behavior.idle_dim_level);
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

/// Say once, per value, that a dim level outside `[0, 1]` was replaced by
/// [`DEFAULT_DIM_LEVEL`]. `--check-config` warns about the same value; this
/// is for the session that never ran it and would otherwise dim to a
/// brightness nobody configured without a word in the log.
fn warn_about_an_unusable_dim_level(dim_level: f32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    // All bits set is a NaN payload no parser produces: the "nothing said
    // yet" mark.
    static WARNED_FOR: AtomicU32 = AtomicU32::new(u32::MAX);
    let bits = dim_level.to_bits();
    if WARNED_FOR.swap(bits, Ordering::Relaxed) == bits {
        return;
    }
    if !(0.0..=1.0).contains(&dim_level) {
        log::warn!(
            "Idle: behavior.idle_dim_level={dim_level} is outside [0, 1]; dimming to \
             {DEFAULT_DIM_LEVEL} instead"
        );
    }
}

fn idle_poll_wakeup(
    enabled: bool,
    clock_available: bool,
    restore_pending: bool,
    last_poll: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<Duration> {
    // Nothing configured, or nothing to measure it with: the only reason
    // left to wake is a dim or a screen-off that still has to be undone.
    if !enabled || !clock_available {
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
            !self.idle.clock_unavailable(),
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

        // The clock before anything else. A backend that cannot measure
        // idleness gets no policy — guessing would dim the screen of somebody
        // who is working — and must keep the display server's own blanker,
        // or a server without the screensaver extension ends up with neither.
        let clock = backend.idle_millis();
        self.idle.note_clock(clock.is_some());
        let Some(idle_millis) = clock else {
            // Whatever an earlier clock dimmed is put back rather than left
            // dark, now that nothing can notice the activity that undoes it.
            let locked = self.features.system_ui.is_locked();
            for action in self.idle.poll(&settings, Duration::ZERO, true, locked, now) {
                self.apply_idle_action(backend, action);
            }
            return;
        };

        // Two idle policies in one session do not share the work, they fight:
        // the X server's blanker resets the very clock read above, so a stage
        // later than the server's own timeout would never be reached. Once
        // this session has a policy, it is the only one.
        if !self.server_saver_suppressed {
            self.server_saver_suppressed = true;
            if backend.suppress_server_screensaver() {
                log::info!("Idle: the display server's own blanking is now off");
            }
        }

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
                    let failure = classify_lock_failure(&error.to_string());
                    let failures = self
                        .idle
                        .note_lock_failed(std::time::Instant::now(), failure);
                    let retry = LOCK_RETRY_INTERVAL.as_secs();
                    // The same predicate the tracker just used, re-derived
                    // from its inputs so the log can never claim a retry the
                    // tracker did not arm.
                    if !lock_failure_retries(failure, failures) {
                        // Nothing more will be tried this idle period: said
                        // once, not once per interval all night.
                        if failure == LockFailure::Permanent {
                            log::warn!(
                                "Idle: cannot lock this session; not retrying until it is next \
                                 idle: {error}"
                            );
                        } else {
                            log::warn!(
                                "Idle: could not lock after {failures} attempts; not retrying \
                                 until this session is next idle: {error}"
                            );
                        }
                    } else if failures == 1 {
                        // Something that passes on its own — a menu holding
                        // the pointer grab — must not leave the session
                        // unlocked until the next time somebody touches the
                        // keyboard. The first failure is worth a warning; a
                        // menu left open all night is not.
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
        idle_status_payload(
            &configured_idle_settings(),
            self.idle_inhibited,
            self.idle.is_dimmed(),
            self.idle.is_screen_off(),
            self.features.system_ui.is_locked(),
        )
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

/// The `get_idle_status` payload. The timeouts are the ones the policy acts
/// on — a lock timeout raised to [`MIN_LOCK_SECS`], a screen-off stage with
/// no command reported as off — rather than the configured numbers, so a bar
/// that counts down to the lock counts down to the lock that will happen.
fn idle_status_payload(
    settings: &IdleSettings,
    inhibited: bool,
    dimmed: bool,
    screen_off: bool,
    locked: bool,
) -> serde_json::Value {
    let secs = |stage: Option<Duration>| stage.map_or(0, |after| after.as_secs());
    serde_json::json!({
        "inhibited": inhibited,
        "dimmed": dimmed,
        "screen_off": screen_off,
        "locked": locked,
        "dim_secs": secs(settings.dim_after),
        "lock_secs": secs(settings.lock_after),
        "screen_off_secs": secs(settings.screen_off_after),
    })
}

/// The lock screen's refusals that no retry can change.
///
/// This is the arm where the compositor reconciler reported *success* and
/// the backend still has no compositor — a capability statement about this
/// backend, true a second from now as well. Matched on the message because a
/// message is all the lock screen sends; the parity test
/// `lock_refusals_are_classified_from_the_messages_the_lock_screen_sends`
/// keeps this list and those messages in step.
const PERMANENT_LOCK_REFUSALS: [&str; 1] =
    ["requires the JWM compositor, and this backend could not start it"];

/// The refusals where the backend *tried* to start a compositor and the
/// attempt returned an error.
///
/// The cause is carried in prose appended after this prefix, and nothing
/// here can read it: a VT switch, a DRM master briefly held by somebody
/// else, a transient GLX failure and a machine with no working renderer at
/// all produce the same shape. Classifying the whole class as permanent is
/// what leaves an unattended session unlocked for the rest of the idle
/// period over something that clears in seconds, so it is retried on a
/// budget instead — see [`LockFailure::Unexplained`].
const UNEXPLAINED_LOCK_REFUSALS: [&str; 1] = ["could not start compositor for"];

fn classify_lock_failure(error: &str) -> LockFailure {
    let says = |refusals: &[&str]| refusals.iter().any(|refusal| error.contains(refusal));
    if says(&PERMANENT_LOCK_REFUSALS) {
        LockFailure::Permanent
    } else if says(&UNEXPLAINED_LOCK_REFUSALS) {
        LockFailure::Unexplained
    } else {
        LockFailure::Transient
    }
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
        assert_eq!(idle_poll_wakeup(false, true, false, None, now), None);
        assert_eq!(
            idle_poll_wakeup(false, true, true, Some(now), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            idle_poll_wakeup(true, true, false, None, now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            idle_poll_wakeup(
                true,
                true,
                false,
                Some(now),
                now + POLL_INTERVAL - Duration::from_nanos(1),
            ),
            Some(Duration::from_nanos(1))
        );
        assert_eq!(
            idle_poll_wakeup(true, true, false, Some(now), now + POLL_INTERVAL),
            Some(Duration::ZERO)
        );
        assert_eq!(
            idle_poll_wakeup(true, true, false, Some(now + Duration::from_secs(2)), now,),
            Some(POLL_INTERVAL)
        );
    }

    #[test]
    fn a_missing_clock_stands_the_poll_timer_down() {
        // A nested backend with no idle clock: waking every second to read
        // nothing keeps the process from ever going quiet.
        let now = std::time::Instant::now();
        assert_eq!(idle_poll_wakeup(true, false, false, Some(now), now), None);
        assert_eq!(idle_poll_wakeup(true, false, false, None, now), None);
        // A dim an earlier clock caused is still worth one wake to undo.
        assert_eq!(
            idle_poll_wakeup(true, false, true, Some(now), now),
            Some(Duration::ZERO)
        );

        let mut tracker = IdleTracker::default();
        assert!(!tracker.clock_unavailable());
        tracker.note_clock(false);
        assert!(tracker.clock_unavailable());
        tracker.note_clock(true);
        assert!(!tracker.clock_unavailable());
    }

    #[test]
    fn the_server_blanker_is_only_taken_away_from_a_session_with_a_clock() {
        // A server without the screensaver extension has no clock to hand
        // over; switching its blanker off first would leave it with neither
        // policy. The probe therefore comes before the suppression.
        const SOURCE: &str = include_str!("idle.rs");
        let poll = SOURCE
            .split_once("fn poll_idle")
            .expect("poll_idle")
            .1
            .split_once("fn reapply_idle_dim")
            .expect("the function after poll_idle")
            .0;
        let probe = format!("{}()", "idle_millis");
        let suppress = format!("{}()", "suppress_server_screensaver");
        let probe_at = poll.find(&probe).expect("poll_idle reads the clock");
        let suppress_at = poll
            .find(&suppress)
            .expect("poll_idle suppresses the server blanker");
        assert!(
            probe_at < suppress_at,
            "poll_idle must read the idle clock before touching the server's blanker"
        );
        let after_probe = &poll[probe_at..suppress_at];
        assert!(
            after_probe.contains("return;"),
            "a missing clock must return before the server's blanker is touched"
        );
    }

    #[test]
    fn idle_status_reports_the_timeouts_the_policy_acts_on() {
        // `idle_lock_secs = 1` locks after the floor, and a screen-off stage
        // without a command never runs: the report says so, as the gate does.
        let settings = IdleSettings::from_secs(120, 0.3, 1, 900, false);
        let payload = idle_status_payload(&settings, true, false, false, false);
        assert_eq!(payload["inhibited"], true);
        assert_eq!(payload["dim_secs"], 120u64);
        assert_eq!(payload["lock_secs"], MIN_LOCK_SECS);
        assert_eq!(payload["screen_off_secs"], 0u64);

        let settings = IdleSettings::from_secs(0, 0.3, 600, 900, true);
        let payload = idle_status_payload(&settings, false, false, false, false);
        assert_eq!(payload["dim_secs"], 0u64);
        assert_eq!(payload["lock_secs"], 600u64);
        assert_eq!(payload["screen_off_secs"], 900u64);
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
        // The grace only bites when the lock timeout is no longer than it, so
        // this is the clamped `idle_lock_secs = 1` case: re-locking a minute
        // after every unlock would still be a session nobody can work in.
        let settings = IdleSettings::from_secs(5, 0.3, 1, 0, false);
        assert_eq!(settings.lock_after, Some(secs(MIN_LOCK_SECS)));
        let mut tracker = IdleTracker::default();
        let start = origin();

        assert_eq!(
            tracker.poll(&settings, secs(MIN_LOCK_SECS), false, false, start),
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
        let during_grace = tracker.poll(
            &settings,
            secs(MIN_LOCK_SECS),
            false,
            false,
            start + secs(50),
        );
        assert!(!during_grace.contains(&IdleAction::Lock));
        // Dimming is not the lock screen and is not held back by the grace.
        assert_eq!(during_grace, [IdleAction::Dim(0.3)]);

        // Once the grace is up, the policy locks again as usual.
        assert!(
            tracker
                .poll(
                    &settings,
                    secs(MIN_LOCK_SECS),
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
        assert_eq!(tracker.note_lock_failed(start, LockFailure::Transient), 1);
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
        assert_eq!(
            tracker.note_lock_failed(start + LOCK_RETRY_INTERVAL, LockFailure::Transient),
            2
        );
        // A lock that finally lands clears the streak, and activity does too.
        tracker.poll(&settings, secs(400), false, true, start + secs(20));
        assert_eq!(
            tracker.note_lock_failed(start + secs(20), LockFailure::Transient),
            1
        );
    }

    #[test]
    fn a_permanent_lock_refusal_waits_for_the_next_idle_period() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();

        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start)
                .contains(&IdleAction::Lock)
        );
        // No compositor to draw the lock screen on, and none could be
        // started: a retry in five seconds would only try to start one again.
        assert_eq!(tracker.note_lock_failed(start, LockFailure::Permanent), 1);
        assert!(
            !tracker
                .poll(
                    &settings,
                    secs(305),
                    false,
                    false,
                    start + LOCK_RETRY_INTERVAL
                )
                .contains(&IdleAction::Lock)
        );
        assert!(
            !tracker
                .poll(&settings, secs(9000), false, false, start + secs(9000))
                .contains(&IdleAction::Lock)
        );
        // The next idle period asks once more, in case the session has a
        // compositor by then.
        tracker.poll(&settings, secs(0), false, false, start + secs(9001));
        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start + secs(9301))
                .contains(&IdleAction::Lock)
        );
    }

    #[test]
    fn lock_refusals_are_classified_from_the_messages_the_lock_screen_sends() {
        assert_eq!(
            classify_lock_failure("another system UI panel is open"),
            LockFailure::Transient
        );
        assert_eq!(
            classify_lock_failure("could not grab pointer for lock screen"),
            LockFailure::Transient
        );
        assert_eq!(
            classify_lock_failure(
                "lock screen requires the JWM compositor, and this backend could not start it"
            ),
            LockFailure::Permanent
        );
        // The backend tried and the attempt failed with a cause this module
        // cannot read. A VT switch produces exactly this, so it must not be
        // the reason an unattended session stops asking to lock.
        assert_eq!(
            classify_lock_failure("could not start compositor for lock screen: no EGL"),
            LockFailure::Unexplained
        );

        // The messages live in the system UI opener. A reworded refusal must
        // fail here rather than quietly turn one class into another.
        const TOGGLES: &str = include_str!("toggles.rs");
        let opener = TOGGLES
            .split_once("fn prepare_system_ui_inner")
            .expect("the system UI opener")
            .1
            .split_once("fn release_temporary_system_ui_compositor")
            .expect("the function after the opener")
            .0;
        for refusal in PERMANENT_LOCK_REFUSALS
            .iter()
            .chain(UNEXPLAINED_LOCK_REFUSALS.iter())
        {
            assert!(
                opener.contains(refusal),
                "prepare_system_ui_inner no longer says {refusal:?}"
            );
        }
        // The two classes must stay distinguishable: a needle that also
        // matched the other arm's message would silently merge them.
        for permanent in PERMANENT_LOCK_REFUSALS {
            assert_eq!(
                classify_lock_failure(permanent),
                LockFailure::Permanent,
                "{permanent:?} is no longer classified as a capability statement"
            );
        }
        for unexplained in UNEXPLAINED_LOCK_REFUSALS {
            assert_eq!(
                classify_lock_failure(unexplained),
                LockFailure::Unexplained,
                "{unexplained:?} is no longer classified as an unread cause"
            );
        }
    }

    /// A backend that tried to start a compositor and failed says so with a
    /// cause nothing here can read. Treating that as permanent is how an
    /// unattended session ends up never locking: a VT switch, or a
    /// compositor restarting under it, produces the same message as a
    /// machine that will never manage it. Retry on a budget, then stop.
    #[test]
    fn an_unreadable_lock_refusal_is_retried_before_it_is_believed() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();

        assert!(
            tracker
                .poll(&settings, secs(300), false, false, start)
                .contains(&IdleAction::Lock)
        );
        assert_eq!(tracker.note_lock_failed(start, LockFailure::Unexplained), 1);
        // Asked again, exactly as a transient refusal would be.
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

        // ...but not for ever: the budget runs out and the idle period ends
        // rather than rebuilding a compositor every five seconds all night.
        let mut at = start + LOCK_RETRY_INTERVAL;
        let mut failures = 1;
        while lock_failure_retries(LockFailure::Unexplained, failures) {
            failures = tracker.note_lock_failed(at, LockFailure::Unexplained);
            at += LOCK_RETRY_INTERVAL;
        }
        assert_eq!(failures, UNEXPLAINED_LOCK_RETRIES);
        assert!(
            !tracker
                .poll(&settings, secs(9000), false, false, at + secs(9000))
                .contains(&IdleAction::Lock),
            "the budget is spent; this idle period is over"
        );

        // The next idle period asks once more, in case the VT came back.
        tracker.poll(&settings, secs(0), false, false, at + secs(9001));
        assert!(
            tracker
                .poll(&settings, secs(300), false, false, at + secs(9301))
                .contains(&IdleAction::Lock)
        );
    }

    /// The retry rule itself, at its edges. A transient refusal is never
    /// abandoned, a capability statement is never retried, and the unknown
    /// cause sits between them with a bounded budget.
    #[test]
    fn the_lock_retry_rule_gives_up_only_where_asking_again_cannot_help() {
        assert!(lock_failure_retries(LockFailure::Transient, 1));
        assert!(lock_failure_retries(LockFailure::Transient, u32::MAX));
        assert!(!lock_failure_retries(LockFailure::Permanent, 1));
        assert!(lock_failure_retries(LockFailure::Unexplained, 1));
        assert!(lock_failure_retries(
            LockFailure::Unexplained,
            UNEXPLAINED_LOCK_RETRIES - 1
        ));
        assert!(!lock_failure_retries(
            LockFailure::Unexplained,
            UNEXPLAINED_LOCK_RETRIES
        ));
        assert!(!lock_failure_retries(LockFailure::Unexplained, u32::MAX));
    }

    #[test]
    fn a_retry_is_dropped_the_moment_the_session_wakes() {
        let settings = settings();
        let mut tracker = IdleTracker::default();
        let start = origin();

        tracker.poll(&settings, secs(300), false, false, start);
        tracker.note_lock_failed(start, LockFailure::Transient);
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
