//! Command-local session environment, without mutating libc's global environ.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Debug, Default)]
pub struct ChildEnvironment {
    values: Arc<Mutex<BTreeMap<OsString, Option<OsString>>>>,
    preserve_host_display_on_restart: bool,
}

impl ChildEnvironment {
    pub fn for_nested_host() -> Self {
        Self {
            preserve_host_display_on_restart: true,
            ..Self::default()
        }
    }

    pub fn set(&self, name: impl Into<OsString>, value: impl Into<OsString>) {
        lock(&self.values).insert(name.into(), Some(value.into()));
    }

    pub fn remove(&self, name: impl Into<OsString>) {
        lock(&self.values).insert(name.into(), None);
    }

    pub fn var_os(&self, name: &str) -> Option<OsString> {
        match lock(&self.values).get(OsStr::new(name)).cloned() {
            Some(value) => value,
            None => std::env::var_os(name),
        }
    }

    pub fn var(&self, name: &str) -> Result<String, std::env::VarError> {
        self.var_os(name)
            .ok_or(std::env::VarError::NotPresent)?
            .into_string()
            .map_err(std::env::VarError::NotUnicode)
    }

    /// Preserve explicit per-command choices, including env_remove and empty
    /// strings. A bar may deliberately disable D-Bus or its input method.
    pub fn apply(&self, command: &mut Command) {
        self.apply_inner(command, false);
    }

    fn apply_for_restart(&self, command: &mut Command) {
        self.apply_inner(command, true);
    }

    fn apply_inner(&self, command: &mut Command, restarting: bool) {
        let explicit: BTreeSet<OsString> =
            command.get_envs().map(|(key, _)| key.to_owned()).collect();
        let values = lock(&self.values).clone();
        for (key, value) in values {
            if explicit.contains(&key)
                || (restarting
                    && self.preserve_host_display_on_restart
                    && (key == "DISPLAY" || key == "WAYLAND_DISPLAY"))
            {
                continue;
            }
            match value {
                Some(value) => {
                    command.env(key, value);
                }
                None => {
                    command.env_remove(key);
                }
            }
        }
    }
}

#[derive(Default)]
struct RegistryState {
    generation: u64,
    active: Option<(u64, ChildEnvironment)>,
}

#[derive(Default)]
struct Registry {
    state: Mutex<RegistryState>,
}

impl Registry {
    fn activate(&self, environment: ChildEnvironment) -> io::Result<ActiveEnvironment<'_>> {
        let mut state = lock(&self.state);
        // JWM's CONFIG and policy loop already represent one application per
        // process. Refuse a second active application instead of mixing its
        // helpers with the first backend's sockets/session bus.
        if state.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "another JWM application environment is active",
            ));
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("application environment generation exhausted"))?;
        let generation = state.generation;
        state.active = Some((generation, environment));
        Ok(ActiveEnvironment {
            registry: self,
            generation,
        })
    }

    fn current(&self) -> Option<ChildEnvironment> {
        lock(&self.state)
            .active
            .as_ref()
            .map(|(_, value)| value.clone())
    }
}

/// Held from pre-setup through teardown and self-exec preparation. Failed
/// setup drops it automatically. Old backend callbacks own a different map.
pub(crate) struct ActiveEnvironment<'a> {
    registry: &'a Registry,
    generation: u64,
}

impl ActiveEnvironment<'_> {
    /// Replace this reservation only after its backend constructor succeeds.
    /// The generation check also prevents a late setup result from publishing
    /// into a newer application's environment.
    pub(crate) fn publish(&self, environment: ChildEnvironment) -> io::Result<()> {
        let mut state = lock(&self.registry.state);
        if state
            .active
            .as_ref()
            .is_none_or(|(generation, _)| *generation != self.generation)
        {
            return Err(io::Error::other(
                "application environment reservation is no longer current",
            ));
        }
        state.active = Some((self.generation, environment));
        Ok(())
    }
}

impl Drop for ActiveEnvironment<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.registry.state);
        if state
            .active
            .as_ref()
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            state.active = None;
        }
    }
}

static ACTIVE: Registry = Registry {
    state: Mutex::new(RegistryState {
        generation: 0,
        active: None,
    }),
};

pub(crate) fn activate(environment: ChildEnvironment) -> io::Result<ActiveEnvironment<'static>> {
    ACTIVE.activate(environment)
}

pub(crate) fn current_or_default() -> ChildEnvironment {
    ACTIVE.current().unwrap_or_default()
}

pub(crate) fn apply(command: &mut Command) {
    if let Some(environment) = ACTIVE.current() {
        environment.apply(command);
    }
}

pub(crate) fn apply_for_restart(command: &mut Command) {
    if let Some(environment) = ACTIVE.current() {
        environment.apply_for_restart(command);
    }
}

pub(crate) fn var_os(name: &str) -> Option<OsString> {
    ACTIVE
        .current()
        .map_or_else(|| std::env::var_os(name), |value| value.var_os(name))
}

pub(crate) fn var(name: &str) -> Result<String, std::env::VarError> {
    var_os(name)
        .ok_or(std::env::VarError::NotPresent)?
        .into_string()
        .map_err(std::env::VarError::NotUnicode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_environment(command: &Command) -> BTreeMap<OsString, Option<OsString>> {
        command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
            .collect()
    }

    #[test]
    fn explicit_command_choices_win_and_parent_environment_is_unchanged() {
        let before = std::env::var_os("JWM_ENV_TEST_ONLY");
        let environment = ChildEnvironment::default();
        environment.set("JWM_ENV_TEST_ONLY", "session");
        environment.set("DBUS_SESSION_BUS_ADDRESS", "session-bus");
        environment.set("GTK_IM_MODULE", "fcitx");
        environment.remove("JWM_ENV_REMOVED_ONLY");
        let mut command = Command::new("sh");
        command.env("DBUS_SESSION_BUS_ADDRESS", "");
        command.env_remove("GTK_IM_MODULE");
        environment.apply(&mut command);
        let values = command_environment(&command);
        assert_eq!(
            values[OsStr::new("JWM_ENV_TEST_ONLY")],
            Some("session".into())
        );
        assert_eq!(
            values[OsStr::new("DBUS_SESSION_BUS_ADDRESS")],
            Some("".into())
        );
        assert_eq!(values[OsStr::new("GTK_IM_MODULE")], None);
        assert_eq!(values[OsStr::new("JWM_ENV_REMOVED_ONLY")], None);
        assert_eq!(std::env::var_os("JWM_ENV_TEST_ONLY"), before);
    }

    #[test]
    fn spawned_child_receives_overlay_without_parent_mutation() {
        let environment = ChildEnvironment::default();
        environment.set("JWM_ENV_TEST_CHILD_ONLY", "owned-fixture");
        let before = std::env::var_os("JWM_ENV_TEST_CHILD_ONLY");
        let mut command = Command::new("sh");
        command.args(["-c", "printf '%s' \"$JWM_ENV_TEST_CHILD_ONLY\""]);
        environment.apply(&mut command);
        let output = command.output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"owned-fixture");
        assert_eq!(std::env::var_os("JWM_ENV_TEST_CHILD_ONLY"), before);
    }

    #[test]
    fn failed_setup_releases_activation_and_old_callbacks_do_not_change_new_backend() {
        let registry = Registry::default();
        let old = ChildEnvironment::default();
        old.set("DISPLAY", ":old");
        {
            let _failed_setup = registry.activate(old.clone()).unwrap();
            assert!(registry.activate(ChildEnvironment::default()).is_err());
            assert_eq!(registry.current().unwrap().var("DISPLAY").unwrap(), ":old");
        }
        assert!(registry.current().is_none());
        let new = ChildEnvironment::default();
        new.set("DISPLAY", ":new");
        let _active = registry.activate(new).unwrap();
        old.set("DISPLAY", ":late-old-callback");
        assert_eq!(registry.current().unwrap().var("DISPLAY").unwrap(), ":new");
    }

    #[test]
    fn reservation_is_exclusive_before_backend_construction_and_publishes_after_success() {
        let registry = Registry::default();
        let reservation = registry.activate(ChildEnvironment::default()).unwrap();
        assert!(registry.activate(ChildEnvironment::default()).is_err());
        let backend = ChildEnvironment::default();
        backend.set("WAYLAND_DISPLAY", "backend-ready");
        reservation.publish(backend).unwrap();
        assert_eq!(
            registry.current().unwrap().var("WAYLAND_DISPLAY").unwrap(),
            "backend-ready"
        );
        drop(reservation);
        assert!(registry.current().is_none());
    }

    #[test]
    fn stale_generation_guard_cannot_remove_newer_state() {
        let registry = Registry::default();
        let stale = registry.activate(ChildEnvironment::default()).unwrap();
        let replacement = ChildEnvironment::default();
        replacement.set("DISPLAY", ":replacement");
        {
            let mut state = lock(&registry.state);
            state.generation += 1;
            state.active = Some((state.generation, replacement));
        }
        assert!(stale.publish(ChildEnvironment::default()).is_err());
        drop(stale);
        assert_eq!(
            registry.current().unwrap().var("DISPLAY").unwrap(),
            ":replacement"
        );
    }

    #[test]
    fn nested_restart_keeps_host_display_while_children_get_private_socket() {
        let environment = ChildEnvironment::for_nested_host();
        environment.set("WAYLAND_DISPLAY", "wayland-own");
        environment.set("DISPLAY", ":own");
        environment.set("XCURSOR_THEME", "cursor-fixture");
        let mut child = Command::new("child");
        environment.apply(&mut child);
        assert_eq!(
            command_environment(&child)[OsStr::new("WAYLAND_DISPLAY")],
            Some("wayland-own".into())
        );
        let mut restart = Command::new("jwm");
        environment.apply_for_restart(&mut restart);
        let values = command_environment(&restart);
        assert!(!values.contains_key(OsStr::new("WAYLAND_DISPLAY")));
        assert!(!values.contains_key(OsStr::new("DISPLAY")));
        assert_eq!(
            values[OsStr::new("XCURSOR_THEME")],
            Some("cursor-fixture".into())
        );
    }

    #[test]
    fn primary_restart_keeps_its_session_overrides() {
        let environment = ChildEnvironment::default();
        environment.set("WAYLAND_DISPLAY", "wayland-own");
        environment.set("DBUS_SESSION_BUS_ADDRESS", "");
        let mut restart = Command::new("jwm");
        environment.apply_for_restart(&mut restart);
        let values = command_environment(&restart);
        assert_eq!(
            values[OsStr::new("WAYLAND_DISPLAY")],
            Some("wayland-own".into())
        );
        assert_eq!(
            values[OsStr::new("DBUS_SESSION_BUS_ADDRESS")],
            Some("".into())
        );
    }
}
