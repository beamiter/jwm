# Seamless X11 viewer input (opt-in)

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
