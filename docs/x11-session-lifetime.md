# X11 upgrade failure and login lifetime

## Confirmed exit path

The XCB and X11RB desktop entries execute `jwm-tool daemon` as the session
command. `JwmManager::is_running` reaps the exact owned child and records its
exit status. Previously, `handle_unexpected_child_exit` then returned from the
daemon for **every** unrequested child exit, including an unsuccessful
replacement executable. A display manager or xinit which ties the X server to
that session command can consequently destroy every application connection.
An independent, always-running Xvfb does not test that lifetime relationship.

This identifies a failure propagation path, not the original October 29
incident's initiating fault. Its linked raw `local-install.json` was unavailable
through the repository contents API during this investigation (404). The
published completion audit remains the evidence for that incident; its missing
stderr cannot be reconstructed.

The current master already removes wallpaper `KillClient` calls based on stale
root-pixmap properties. See [restart state](restart-state.md). That fix prevents
one independently reproduced connection-loss path; neither fix establishes the
original incident's root cause.

## Minimal containment

For an explicitly selected `xcb` or `x11rb` backend, a nonzero exit, signal, or
unknown child wait status now leaves the daemon and control FIFO alive. The
failure is reported once, the child is reaped, and `status`, `start`, `stop`, and
`quit` remain usable. There is no automatic restart loop against a bad binary.
A zero exit retains the existing logout behavior. Explicit daemon `quit` and
termination still stop the child and end the session. Wayland and unspecified
backend behavior are unchanged.

This preserves the session lifetime owner; it does not keep a dead WM serving
input, resurrect an X server, or recover a client killed directly. Recovery may
need a separate terminal/TTY or remote connection. With the same session's
`XDG_RUNTIME_DIR`, `DISPLAY`, `XAUTHORITY`, and backend settings, repair or roll
back the executable and run `jwm-tool start`. Use `jwm-tool quit` to intentionally
end the session. Do not use force-restarting the session daemon as recovery.

**An already-running old daemon does not receive this fix through WM exec.**
The corrected daemon must be installed and started in a separate safe test
login first. This change is not authorization or a mechanism for hot-replacing
the user's current session supervisor.

## Other audited ownership boundaries

- IPC/hotkey `restart` sets the restart flag; application lifecycle preflights
  configuration, window discovery and snapshot publication before cleanup.
- Successful restart uses same-process exec. Exec failure retries bootstrap in
  the original process; exhausted bootstrap errors can still exit nonzero.
- Shared-shell cleanup targets its owned child through
  `terminate_secondary_bar_child`, rather than the login session process group.
- X11 window adoption and one-shot snapshot restoration remain unchanged.
  Normal exec preserves tags/layouts under that existing protocol. Starting a
  fresh WM after a crash does not inherit the old process's one-shot intent;
  full crash-time layout restoration is not promised here.
- No top-bar frontend, shared-shell protocol or configuration schema changes.

## Regression

`scripts/test-x11-session-lifetime.py` owns private Xvfb, HOME/XDG paths, IPC,
D-Bus and child processes. Its lifetime watcher terminates only its owned X
server when the session command exits, modeling the missing display-manager
relationship. It never selects the user's display. Logs and JSON are retained
in the reported temporary directory, including on failure.

```sh
cargo test --locked -p jwm --no-default-features \
  --features backend-xcb,backend-x11rb --bin jwm-tool
python3 scripts/test-x11-session-lifetime.py \
  --jwm target/debug/jwm --tool target/debug/jwm-tool \
  --report /tmp/x11-session-lifetime.json
# Also exercise shared-shell recovery with actual built host/header binaries:
python3 scripts/test-x11-session-lifetime.py \
  --jwm target/debug/jwm --tool target/debug/jwm-tool \
  --shell crates/jwm_shell/target/debug/jwm-shell \
  --bar bars/x11rb_bar/target/debug/x11rb_bar \
  --report /tmp/x11-session-lifetime-shell.json
```

The fixture checks normal restart PID/XID identity, five applications on tags
1/3/8, selected tag and layout, and optional host readiness. It then atomically
switches its private install symlink to `/bin/false`, requests exec restart,
checks that the failed replacement does not end the session or its applications,
restores the symlink, starts the WM and checks adoption. Finally explicit quit
must end the modeled login lifetime. This is an isolated process-lifetime
regression, not a real display-manager/PAM/logind or physical Xorg acceptance.

Physical Wayland, external monitors and fractional scaling remain independent
open acceptance items. The original resident-session failure remains open.

## Validation in this change (2026-10-30)

Base: `f5f8d5965a431d831c448d6f23ed5775689fc7ba`; Rust 1.99.1.
The build uses `--no-default-features --features backend-xcb,backend-x11rb`.

| Check | Observed result |
|---|---|
| `jwm-tool` build | Passed |
| Strict Clippy (`--bin jwm-tool -- -D warnings`) and rustfmt | Passed |
| Python fixture syntax / CLI | Passed; display execution blocked below |
| Targeted `x11_` tests | 5 passed, including 3 new lifecycle tests |
| Complete `jwm-tool` suite, patched | 150 passed, 7 failed |
| Same suite, unmodified base | 147 passed, the same 7 failed |
| Unmodified baseline daemon negative control | Failed as expected: exec-to-failure ended the XCB daemon |
| Real daemon / disposable child | XCB passed failure containment and start/stop/quit; X11RB and Wayland cases could not start because `/proc` identity for their PID was unavailable |
| Private X11 lifetime fixture | Blocked before WM startup: Xvfb could not create local/unix listening sockets in this sandbox |
| Physical login, five windows, Tag/layout, shared-shell recovery | Not validated in this environment |

The seven failures are four socket-dependent `nested_smoke` tests and three
`/proc` identity/cmdline/sample tests. The identical baseline failure set is
recorded to distinguish environment restrictions from this change; it is not a
full-suite pass. The process-only regression is reproducible with:

```sh
python3 scripts/test-x11-daemon-recovery.py --tool target/debug/jwm-tool \
  --report /tmp/x11-daemon-recovery.json
```

This uses the real daemon with a disposable exec-to-failure child, not a real
window manager or display. Both regression scripts retain failure evidence.
No current user desktop was restarted. Real display-manager/session acceptance
is still required before treating the original migration incident as resolved.
