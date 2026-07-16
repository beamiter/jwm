//! Process-backed Linux desktop actions for `xbar_core` hosts.
//!
//! This companion crate deliberately stays outside `xbar_core`: executable
//! names, process spawning, and child reaping are host policy rather than bar
//! model semantics.

use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    process::{Child, Command},
    sync::mpsc,
    thread,
};

use xbar_core::{BarEffect, PlatformEffectHandler};

/// One executable and its fixed argument list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
}

/// Configurable process policy for the effects this adapter understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessActionConfig {
    pub screenshot: Option<CommandSpec>,
    pub audio_control: Option<CommandSpec>,
    pub waiter_thread_prefix: String,
}

impl Default for ProcessActionConfig {
    fn default() -> Self {
        Self {
            screenshot: Some(CommandSpec::new("flameshot").with_arg("gui")),
            audio_control: Some(CommandSpec::new("pavucontrol")),
            waiter_thread_prefix: "xbar-wait".to_owned(),
        }
    }
}

/// Failure to accept or launch a process-backed effect.
#[derive(Debug)]
pub enum ProcessActionError {
    UnsupportedEffect(BarEffect),
    DisabledEffect(BarEffect),
    Spawn {
        program: OsString,
        source: io::Error,
    },
    SpawnWaiter {
        program: OsString,
        source: io::Error,
    },
    WaiterStopped {
        program: OsString,
    },
}

impl fmt::Display for ProcessActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEffect(effect) => {
                write!(f, "process action adapter does not handle {effect:?}")
            }
            Self::DisabledEffect(effect) => {
                write!(f, "process command is disabled for {effect:?}")
            }
            Self::Spawn { program, source } => {
                write!(
                    f,
                    "failed to launch {}: {source}",
                    program.to_string_lossy()
                )
            }
            Self::SpawnWaiter { program, source } => write!(
                f,
                "failed to start child waiter for {}: {source}",
                program.to_string_lossy()
            ),
            Self::WaiterStopped { program } => write!(
                f,
                "child waiter stopped before accepting {}",
                program.to_string_lossy()
            ),
        }
    }
}

impl std::error::Error for ProcessActionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::SpawnWaiter { source, .. } => Some(source),
            Self::UnsupportedEffect(_) | Self::DisabledEffect(_) | Self::WaiterStopped { .. } => {
                None
            }
        }
    }
}

/// `PlatformEffectHandler` for screenshot and audio-control process effects.
#[derive(Debug, Clone, Default)]
pub struct ProcessActionHandler {
    config: ProcessActionConfig,
}

impl ProcessActionHandler {
    #[must_use]
    pub const fn new(config: ProcessActionConfig) -> Self {
        Self { config }
    }

    #[must_use]
    pub const fn config(&self) -> &ProcessActionConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut ProcessActionConfig {
        &mut self.config
    }

    #[must_use]
    pub const fn supports(effect: BarEffect) -> bool {
        matches!(effect, BarEffect::Screenshot | BarEffect::OpenAudioControl)
    }

    pub fn command_for_effect(
        &self,
        effect: BarEffect,
    ) -> Result<&CommandSpec, ProcessActionError> {
        let command = match effect {
            BarEffect::Screenshot => &self.config.screenshot,
            BarEffect::OpenAudioControl => &self.config.audio_control,
            _ => return Err(ProcessActionError::UnsupportedEffect(effect)),
        };
        command
            .as_ref()
            .ok_or(ProcessActionError::DisabledEffect(effect))
    }

    /// Launch a configured command and transfer child ownership to a bounded
    /// waiter thread so the host never leaks a zombie process.
    pub fn launch(&self, command: &CommandSpec) -> Result<(), ProcessActionError> {
        let program = command.program.clone();
        let thread_name = waiter_name(&self.config.waiter_thread_prefix, &program);
        let (sender, receiver) = mpsc::sync_channel::<(OsString, Child)>(1);
        thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                if let Ok((program, mut child)) = receiver.recv() {
                    reap_child(&program, &mut child);
                }
            })
            .map_err(|source| ProcessActionError::SpawnWaiter {
                program: command.program.clone(),
                source,
            })?;

        let child = Command::new(&command.program)
            .args(&command.args)
            .spawn()
            .map_err(|source| ProcessActionError::Spawn {
                program: command.program.clone(),
                source,
            })?;

        if let Err(error) = sender.send((program.clone(), child)) {
            let (_, mut child) = error.0;
            reap_child(&program, &mut child);
            return Err(ProcessActionError::WaiterStopped { program });
        }
        Ok(())
    }
}

impl PlatformEffectHandler for ProcessActionHandler {
    type Error = ProcessActionError;

    fn handle(&mut self, effect: BarEffect) -> Result<(), Self::Error> {
        let command = self.command_for_effect(effect)?.clone();
        self.launch(&command)
    }
}

fn waiter_name(prefix: &str, program: &OsStr) -> String {
    format!("{prefix}-{}", program.to_string_lossy())
}

fn reap_child(program: &OsStr, child: &mut Child) {
    match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => log::warn!("{} exited with {status}", program.to_string_lossy()),
        Err(error) => log::warn!(
            "failed to reap {} process: {error}",
            program.to_string_lossy()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_map_only_the_two_process_effects() {
        let handler = ProcessActionHandler::default();
        assert_eq!(
            handler.command_for_effect(BarEffect::Screenshot).unwrap(),
            &CommandSpec::new("flameshot").with_arg("gui")
        );
        assert_eq!(
            handler
                .command_for_effect(BarEffect::OpenAudioControl)
                .unwrap(),
            &CommandSpec::new("pavucontrol")
        );
        assert!(ProcessActionHandler::supports(BarEffect::Screenshot));
        assert!(!ProcessActionHandler::supports(BarEffect::ToggleMute));
        assert!(matches!(
            handler.command_for_effect(BarEffect::ToggleMute),
            Err(ProcessActionError::UnsupportedEffect(BarEffect::ToggleMute))
        ));
    }

    #[test]
    fn configured_effect_can_be_disabled_without_affecting_the_other() {
        let handler = ProcessActionHandler::new(ProcessActionConfig {
            screenshot: None,
            ..ProcessActionConfig::default()
        });

        assert!(matches!(
            handler.command_for_effect(BarEffect::Screenshot),
            Err(ProcessActionError::DisabledEffect(BarEffect::Screenshot))
        ));
        assert!(
            handler
                .command_for_effect(BarEffect::OpenAudioControl)
                .is_ok()
        );
    }

    #[test]
    fn launch_reports_a_missing_executable_synchronously() {
        let handler = ProcessActionHandler::default();
        let error = handler
            .launch(&CommandSpec::new(
                "/definitely/missing/xbar-linux-actions-test",
            ))
            .unwrap_err();

        assert!(matches!(error, ProcessActionError::Spawn { .. }));
        assert!(error.to_string().contains("failed to launch"));
    }

    #[test]
    fn platform_handler_launches_and_reaps_a_real_child() {
        let mut handler = ProcessActionHandler::new(ProcessActionConfig {
            screenshot: Some(CommandSpec::new("/bin/true")),
            audio_control: None,
            waiter_thread_prefix: "xbar-actions-test".to_owned(),
        });

        PlatformEffectHandler::handle(&mut handler, BarEffect::Screenshot).unwrap();
    }
}
