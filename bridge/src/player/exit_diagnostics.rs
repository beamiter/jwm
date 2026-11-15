//! Opt-in, fixed-size process evidence. Never includes paths or media data.
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Instant;

pub(super) const ENV: &str = "JWM_PLAYER_EXIT_DIAGNOSTICS";

#[derive(Clone, Copy)]
pub(super) enum Phase {
    Spawned,
    IpcEof,
    QuitRequested,
    RequestsDisconnected,
    ExitObserved,
    CleanupStarted,
    KillRequested,
    CleanupExitObserved,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Spawned => "spawned",
            Self::IpcEof => "ipc_eof",
            Self::QuitRequested => "quit_requested",
            Self::RequestsDisconnected => "requests_disconnected",
            Self::ExitObserved => "exit_observed",
            Self::CleanupStarted => "cleanup_started",
            Self::KillRequested => "kill_requested",
            Self::CleanupExitObserved => "cleanup_exit_observed",
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Record {
    pub phase: &'static str,
    pub player_pid: u32,
    pub mpv_pid: u32,
    pub elapsed_ms: u128,
    pub first_eof_ms: Option<u128>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

pub(super) struct Diagnostics {
    enabled: bool,
    seen: u8,
    started: Instant,
    player_pid: u32,
    mpv_pid: u32,
    first_eof_ms: Option<u128>,
}

impl Diagnostics {
    pub fn new(enabled: bool, mpv_pid: u32) -> Self {
        Self {
            enabled,
            seen: 0,
            started: Instant::now(),
            player_pid: std::process::id(),
            mpv_pid,
            first_eof_ms: None,
        }
    }

    pub fn observe(&mut self, phase: Phase, status: Option<&ExitStatus>) -> Option<Record> {
        self.observe_at(phase, status, self.started.elapsed().as_millis())
    }

    fn observe_at(
        &mut self,
        phase: Phase,
        status: Option<&ExitStatus>,
        elapsed_ms: u128,
    ) -> Option<Record> {
        // Eight static phases, each recorded once. No unbounded event list,
        // polling, additional waitpid, media strings, or stderr capture.
        let bit = 1u8 << phase as u8;
        if !self.enabled || self.seen & bit != 0 {
            return None;
        }
        self.seen |= bit;
        if matches!(phase, Phase::IpcEof) {
            self.first_eof_ms = Some(elapsed_ms);
        }
        Some(Record {
            phase: phase.label(),
            player_pid: self.player_pid,
            mpv_pid: self.mpv_pid,
            elapsed_ms,
            first_eof_ms: self.first_eof_ms,
            exit_code: status.and_then(ExitStatus::code),
            signal: status.and_then(ExitStatusExt::signal),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_records_nothing() {
        let mut diagnostics = Diagnostics::new(false, 42);
        assert_eq!(diagnostics.observe_at(Phase::IpcEof, None, 10), None);
        assert_eq!(diagnostics.seen, 0);
    }

    #[test]
    fn actual_child_exit_is_recorded_without_polling_again() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let mut diagnostics = Diagnostics::new(true, child.id());
        let status = child.wait().unwrap();
        let record = diagnostics
            .observe(Phase::ExitObserved, Some(&status))
            .unwrap();
        assert_eq!(record.mpv_pid, child.id());
        assert_eq!((record.exit_code, record.signal), (Some(7), None));
        assert!(
            diagnostics
                .observe(Phase::ExitObserved, Some(&status))
                .is_none()
        );
    }

    #[test]
    fn actual_cleanup_signal_cannot_be_mistaken_for_pre_cleanup_exit() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut diagnostics = Diagnostics::new(true, child.id());
        diagnostics.observe(Phase::CleanupStarted, None).unwrap();
        diagnostics.observe(Phase::KillRequested, None).unwrap();
        child.kill().unwrap();
        let status = child.wait().unwrap();
        let record = diagnostics
            .observe(Phase::CleanupExitObserved, Some(&status))
            .unwrap();
        assert_eq!(record.phase, "cleanup_exit_observed");
        assert_eq!((record.exit_code, record.signal), (None, Some(9)));
    }

    #[test]
    fn eof_is_first_observation_and_not_an_exit_claim() {
        let mut diagnostics = Diagnostics::new(true, 42);
        let eof = diagnostics.observe_at(Phase::IpcEof, None, 10).unwrap();
        assert_eq!(eof.first_eof_ms, Some(10));
        assert_eq!(eof.exit_code, None);
        assert_eq!(eof.signal, None);
        assert_eq!(diagnostics.observe_at(Phase::IpcEof, None, 20), None);
        let cleanup = diagnostics
            .observe_at(Phase::CleanupStarted, None, 30)
            .unwrap();
        assert_eq!(cleanup.first_eof_ms, Some(10));
    }

    #[test]
    fn exit_code_and_signal_are_distinct_and_cleanup_is_labeled() {
        let mut diagnostics = Diagnostics::new(true, 42);
        let status = ExitStatus::from_raw(7 << 8);
        let exit = diagnostics
            .observe_at(Phase::ExitObserved, Some(&status), 3)
            .unwrap();
        assert_eq!((exit.exit_code, exit.signal), (Some(7), None));
        let status = ExitStatus::from_raw(9);
        let exit = diagnostics
            .observe_at(Phase::CleanupExitObserved, Some(&status), 4)
            .unwrap();
        assert_eq!((exit.exit_code, exit.signal), (None, Some(9)));
        assert_eq!(exit.phase, "cleanup_exit_observed");
    }

    #[test]
    fn all_phases_emit_at_most_eight_records() {
        let phases = [
            Phase::Spawned,
            Phase::IpcEof,
            Phase::QuitRequested,
            Phase::RequestsDisconnected,
            Phase::ExitObserved,
            Phase::CleanupStarted,
            Phase::KillRequested,
            Phase::CleanupExitObserved,
        ];
        let mut diagnostics = Diagnostics::new(true, 42);
        for phase in phases {
            assert!(diagnostics.observe_at(phase, None, 1).is_some());
            assert!(diagnostics.observe_at(phase, None, 2).is_none());
        }
        assert_eq!(diagnostics.seen, u8::MAX);
    }
}
