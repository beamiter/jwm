//! Authoritative JWM lock/resume facts, independent of compositor body mode.
//! Same-UID is not a hostile-process sandbox; the daemon additionally binds
//! this role to its explicitly configured JWM executable and kernel peer PID.
use companion_client::Client;
use companion_protocol::{Reply, Request};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
static CONNECTED: AtomicBool = AtomicBool::new(false);
pub(crate) fn connected() -> bool {
    CONNECTED.load(Ordering::Acquire)
}
static ENABLED: AtomicBool = AtomicBool::new(false);
static HAVE_STATE: AtomicBool = AtomicBool::new(false);
static LOCKED: AtomicBool = AtomicBool::new(true);
pub(crate) fn publish_locked(locked: bool) {
    LOCKED.store(locked, Ordering::Release);
    HAVE_STATE.store(true, Ordering::Release);
}
pub(crate) fn start() {
    if ![
        "JWM_COMPANION_OBSERVER",
        "JWM_COMPANION_SCENE",
        "JWM_EVA_COMPANION",
    ]
    .iter()
    .any(|key| std::env::var(key).as_deref() == Ok("1"))
        || std::env::var_os("SSH_CONNECTION").is_some()
    {
        return;
    }
    let (Ok(session), Some(runtime)) = (
        std::env::var("XDG_SESSION_ID"),
        std::env::var_os("XDG_RUNTIME_DIR"),
    ) else {
        return;
    };
    if ENABLED.swap(true, Ordering::AcqRel) {
        return;
    }
    let path = std::env::var_os("EVA_COMPANION_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(runtime).join("jcompanion/eva.sock"));
    let _ = std::thread::Builder::new()
        .name("jwm-companion-desktop".into())
        .spawn(move || {
            let source = format!("jwm-desktop-{}", std::process::id());
            let mut connection: Option<Client> = None;
            let mut accepted = false;
            let mut retry = Instant::now();
            let mut sent = None;
            let mut offset = suspend_offset_ns();
            let mut resume_pending = false;
            loop {
                if let Some(now) = suspend_offset_ns() {
                    if offset.is_some_and(|old| resume_observed(old, now)) {
                        resume_pending = true;
                    }
                    offset = Some(now);
                }
                if connection.is_none() && Instant::now() >= retry {
                    match Client::connect_desktop(&path, session.clone(), source.clone()) {
                        Ok(c) => connection = Some(c),
                        Err(_) => retry = Instant::now() + Duration::from_secs(2),
                    }
                }
                let mut lost = false;
                if let Some(client) = connection.as_mut() {
                    match client.poll() {
                        Ok(replies) => {
                            for reply in replies {
                                match reply {
                                    Reply::Accepted { .. } => {
                                        accepted = true;
                                        CONNECTED.store(true, Ordering::Release);
                                        sent = None;
                                    }
                                    Reply::Rejected { .. } => lost = true,
                                    _ => {}
                                }
                            }
                        }
                        Err(_) => lost = true,
                    }
                    if accepted && !lost && HAVE_STATE.load(Ordering::Acquire) {
                        let locked = LOCKED.load(Ordering::Acquire);
                        // On reconnection lock state precedes resume. An unknown
                        // connection is never treated as evidence of unlock.
                        if sent != Some(locked) {
                            if client.send(Request::SessionLock { locked }).is_ok() {
                                sent = Some(locked);
                            } else {
                                lost = true;
                            }
                        }
                        if !lost && resume_pending {
                            if client.send(Request::Resume).is_ok() {
                                resume_pending = false;
                            } else {
                                lost = true;
                            }
                        }
                    }
                }
                if lost {
                    CONNECTED.store(false, Ordering::Release);
                    connection = None;
                    accepted = false;
                    sent = None;
                    retry = Instant::now() + Duration::from_secs(2);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
}
fn clock_ns(clock: libc::clockid_t) -> Option<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(clock, &mut value) } != 0
        || value.tv_sec < 0
        || value.tv_nsec < 0
    {
        return None;
    }
    (value.tv_sec as u64)
        .checked_mul(1_000_000_000)?
        .checked_add(value.tv_nsec as u64)
}
/// BOOTTIME includes suspended time; MONOTONIC excludes it. Reject a sampled
/// pair spanning scheduler delay instead of mistaking a slow loop for resume.
fn suspend_offset_ns() -> Option<u64> {
    let before = clock_ns(libc::CLOCK_MONOTONIC)?;
    let boot = clock_ns(libc::CLOCK_BOOTTIME)?;
    let after = clock_ns(libc::CLOCK_MONOTONIC)?;
    if after.checked_sub(before)? > 1_000_000 {
        return None;
    }
    boot.checked_sub(before + (after - before) / 2)
}
fn resume_observed(old: u64, new: u64) -> bool {
    new.saturating_sub(old) >= 2_000_000
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_kernel_suspend_offset_not_loop_elapsed_causes_resume() {
        assert!(!resume_observed(100, 100));
        assert!(!resume_observed(100, 900));
        assert!(resume_observed(100, 2_000_100));
        assert!(!resume_observed(1000, 100));
    }
}
