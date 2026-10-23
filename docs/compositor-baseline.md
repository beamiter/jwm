# Compositor policy baseline

Run `scripts/test-compositor-baseline.sh backend-x11rb` (or `backend-xcb`).
`JWM_BASELINE_REPEATS` defaults to five and accepts 1–100. Every failure is
fatal; the script never retries until green, ignores a test, or changes an
assertion. It uses the existing child-process cleanup wrapper. The selected
status-bar, gesture and monitor-transfer cases run together on eight libtest
threads. Keep the complete library and headless GL suites as separate gates.

## 2026-10-23 investigation

Base: `a9a7614516a05f58ca579cf43ed9cf7c38d4ecb0`; Rust 1.97.1; Linux x86_64.
The repository had six open dependency/CI PRs and no open issues.

- The commit message reports three status-bar failures, but gives no test names.
  On this base, all 12 `statusbar::tests` pass, both independently and in the
  complete X11 suite. Those historical failures are **not reproduced** here;
  they are not evidence of a defect in the wallpaper change.
- `queued_swipe_cannot_act_after_lock_or_configuration_change` passes both
  independently and in the complete suite. No gesture behavior was changed.
- `visible_fullscreen_sendmon_fills_the_target_and_returns_to_it` fails both
  independently and in the complete suite: expected y=92, actual y=144. The
  extra 52 pixels equal the default padded status-bar reservation. This is a
  deterministic baseline geometry defect, not a display-server or GPU failure.
  The separate fullscreen restore fix keeps migration coordinates relative to
  the user's bar preference rather than temporary fullscreen suppression.

The original X11 run: 5677 passed, one failed, one ignored. With the separate
fix and its round-trip regression: 5679 passed, none failed, one ignored.
The ignored test retains its existing opt-in real-X11 requirements.

The dedicated CI step exercises X11RB, XCB and the combined backend profile.
These policy tests need no real display and make no claim about physical
monitor hotplug, DRM/KMS, presentation timing or driver correctness.
