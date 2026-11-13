# Seamless viewer input (explicit opt-in)

## X11 viewer

```
jwm-remote connect HOST --grab-input --seamless-input
jwm-remote ssh USER@HOST --grab-input --seamless-input
```

The host must authorize input and negotiate both pointer and compatible keyboard
control. This option grants no new remote permission and is never saved in a
connection profile. Ordinary click-to-grab and view-only behavior are unchanged.

Move into the displayed remote image to acquire input without a click. Move out
of that image, including into letterboxing, to release the local grab and all
forwarded remote keys/buttons. The pointer is never confined. The pre-existing
local focus is not rewritten. The title indicates the current input mode.

Entry waits until physical keys and core mouse buttons are released, so a local
chord or drag is not transferred halfway through. Ctrl+Super+F12 (or the configured
escape key) releases input and suspends re-entry until the pointer leaves the
image. Focus loss, unmapping and resizing release input; leave and enter again
to resume. Source switching also suspends automatic acquisition. Closing or
losing the connection retains the existing release-all teardown path.

This first implementation targets an X11 viewer and X11 host. Native Wayland's
scoped, one-shot authorizations still revoke on surface leave and are never
silently renewed. Desktop/compositor-reserved shortcuts are not guaranteed to be
forwarded. Mouse-follow input is an explicit per-session choice: hovering the
remote image can capture typing even when another local window had focus.

A drag across the edge is deliberately ended remotely, not continued into the
local application. Extra mouse buttons beyond the core X11 button state and
multi-seat interaction are not part of this mode's tested contract.

## Regression

The `Remote window safety` workflow runs the owned-Xvfb crossing regression. To
run it on a dedicated test server (never a user's live display):

```
xvfb-run -a -s '-screen 0 800x600x24 -nolisten tcp' sh -ec '
  export JWM_TEST_X11_DISPLAY="$DISPLAY" JWM_REMOTE_TEST_VIEWER_OWNED=1
  cargo test --locked --no-default-features --features remote-x11 --lib \
    seamless_input_crossing_releases_and_escape_requires_exit_on_x11 \
    -- --ignored --test-threads=1 --nocapture
'
```

## Experimental native Wayland (JWM udev)

This is a separate opt-in from X11 `--seamless-input`. Both machines must run
compatible JWM native protocols, and the viewer must run under **JWM's udev
backend**. It does not enable focus-following on arbitrary Wayland compositors
or nested JWM, and it never falls back to X11. Missing permissions or protocol
support cause an error rather than silently selecting another input mode.
Update **and restart the local JWM compositor**, not only `jwm-remote`. The
viewer requires local handoff protocol version 2, which guarantees scene
hit-testing of the pointer and position-aware geometry guards. An older
version-1 compositor is refused; no fallback is attempted.

**Entering the displayed remote image can redirect your typing and ordinary
keyboard shortcuts away from the previous local window.** Both peers must
explicitly authorize this behavior for this one session. Defaults remain off.

Example using an existing, securely provisioned shared key file and a manual
SSH tunnel. Run the host in the source machine's JWM session, select the intended
window when prompted, and keep that source window focused. Do not put key
contents in command arguments or logs. Paths below are placeholders for each
machine's private copy of the same key.

```sh
# Source machine: trusted local selection, loopback listener, one connection.
jwm-remote host --wayland-select --once \
  --allow-wayland-control --allow-resumable-control \
  --listen 127.0.0.1:48221 --key-file /private/session.key

# Viewer machine, separate terminal: keep this SSH port forward running.
ssh -N -L 48221:127.0.0.1:48221 user@SOURCE_HOST

# Viewer machine, under JWM udev:
jwm-remote connect 127.0.0.1:48221 \
  --viewer-backend wayland --wayland-control --wayland-seamless-input \
  --key-file /private/session.key
```

The dedicated client flag requests resumable combined control. There is no
separate client resumable flag to add. The host's `--allow-resumable-control`
is required in addition to ordinary `--allow-wayland-control`; neither side's
permission alone is sufficient. Do not combine this with `--grab-input` or
X11 `--seamless-input`. The `jwm-remote ssh` subcommand and saved profiles do
**not** support the native seamless option. The manual `ssh -N -L` command above
only supplies a tunnel; it does not launch or authorize the remote host.

### What entry and exit mean

- Entry waits for clean physical input, actual local compositor focus and a
  presented remote frame. It does not immediately authorize input from an
  unacknowledged transition.
- Leaving the image immediately stops forwarding and releases local shortcut
  inhibition. A native release acknowledgement must confirm remote held-key
  cleanup before any resume. Keys still physically held are quarantined until
  release, not replayed into the previous local application. The pointer is
  never confined.
- Re-entry resumes only the same original authorization, with fresh checks and
  presentation proof. **The absolute lifetime is at most 30 seconds and never
  renews on pause, hover or resume.** Expiry is terminal; there is no automatic
  reconnect or new lease. Start a new explicitly authorized session if needed.
- Crossing into black bars releases control, but v1 uses a surface-entry token:
  you must leave the entire viewer surface and enter it again before resuming.
  Moving from a black bar straight back into the image is insufficient.
- **Ctrl+Super+F12 permanently releases this session.** It is not X11's temporary
  escape latch; leaving and re-entering cannot restore it. Configured JWM
  lock-screen and lock-monitor shortcuts remain local while control is active.
  The native keyboard allowlist does **not** permit Super: pressing Super ends
  remote control instead of forwarding it. The emergency chord remains local,
  but the session may already have ended at its Super press. Do not expect all
  desktop/system shortcuts to pass through.
- Unrelated focus loss, lock, source/viewer geometry changes (including window
  position), keymap/seat changes,
  malformed transitions, failed checks and disconnection are terminal. Normal
  hover exit is accepted only as an ordered compositor transaction, not as a
  reason to treat arbitrary focus loss as resumable. Window movement or
  occlusion can change the pointer's actual recipient even while the physical
  mouse is stationary; this must not retain control of the old recipient.
- Crossing monitors can trigger a non-transactional WM focus change and end
  the session. Resumable cross-monitor handoff is not guaranteed. Startup with
  the cursor already inside uses Smithay's normal enter delivery, but that
  startup ordering has not had dedicated acceptance testing.

No drag or modifier-click is supported. Keyboard and pointer actions remain
sequential, not overlapping gestures. Compositor modifier+mouse bindings are
not suppressed and can terminate control through a focus or geometry change.
Clipboard, static-lossless repair, resize/follow-size, stored permissions and
multi-seat operation are not part of this native seamless mode. Without the
native flag, existing one-shot Wayland control still revokes on surface leave.

### Validation boundary

Owned headless JWM compositor tests exercise real native viewer presentation,
authenticated TCP, native pause/release ACKs, held-key cleanup, fast re-entry and
terminal focus loss. They also check that the same lease and deadline survive
pause/resume. These tests do **not** establish physical mouse/keyboard, live
desktop, DRM/GPU or two-machine acceptance. Parser-to-options tests and actual
production-viewer TCP tests provide **separate layers of evidence**. The latter
construct options in the fixture; they do not launch the CLI as an independent
process.

The separate `scripts/test-remote-wayland-cli.sh` CI gate builds and launches
actual `jwm-remote host` and `connect` processes against two owned compositors.
It requires all three cases to execute with strict GL and no skipped tests:
real input/pause/resume under the same lease followed by successful process
cleanup, missing host permission rejection, and old local protocol rejection.
Readiness is observed only after the real ACK, local fence and fresh proof;
recipient events and lease identity are checked separately. Unix socket or GL
unavailability fails this gate rather than turning it into a passing skip.
Passing this owned-software process gate is still not physical desktop acceptance.

See the [native integration contract](remote-wayland-seamless-design.md) for
protocol and state-machine details. This remains experimental, not a generic
Wayland remote-desktop guarantee.
