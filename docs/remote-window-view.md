# X11 application-window viewing (experimental)

This is the first, deliberately **view-only** window-scoped slice of the remote
application plan. It does not implement native remote resize or exclusive input.
It reuses the authenticated/encrypted remote transport and ordinary viewer.

## Local authorization

On the host, choose the application's client window with `xwininfo`, then run:

```sh
jwm-remote host --window 0x200001 --once --key-file /path/to/session.key
```

The XID is an example, not a persistent application identifier. Run this command
locally for each new grant. Use an SSH tunnel to the default loopback listener and
the existing `jwm-remote connect` viewer. Do not publish a key in shell history,
logs or benchmark artifacts. The automatic `jwm-remote ssh` launcher does not yet
issue window grants; start the authorized host explicitly instead.

The host binds the window instance **before** printing its listening address.
It retains that X connection and named Composite pixmap while waiting for an
authenticated viewer. The grant is consumed once, even when the session fails.
Destroy, unmap or reparent of the window **or any ancestor**, and native resize
of the selected window, revoke the grant. Ancestor lifecycle notifications are
subscribed before authorization, so a brief ancestor unmap followed by map
between frame checks still revokes permanently. Starting another
window with the same title or a reused XID does not restore it. Restart the host
and explicitly select the current application to grant again.

This first backend supports mapped, borderless, opaque 24-bit TrueColor application windows with an
active X11 compositor and XFixes selection notifications.
The window must already be redirected by the compositor. Unsupported windows
or missing XComposite support fail closed. It reads the
window's named pixmap, never a root rectangle, and never falls back to the root
or compositor overlay. It does not sample or transmit the global cursor.
Occlusion by another application must not reveal that application's pixels.
Popups outside the selected client window are not implicitly granted.

`--window` requires `--once` and rejects input, clipboard and additional sources.
These restrictions also apply to programmatic host callers. XTEST control would
be global, so silently enabling it for a window would not be a safe approximation
of window-scoped control. X11 itself is not an isolation boundary against other
local X clients.

Viewer resizing changes encoded image bounds only, not the original application
size. Native resizing, input ownership, focus/lock handling, reconnect recovery
and Wayland window grants remain separate follow-up work.

## Required validation before deployment

Automated policy/state tests cover invalid grants and terminal revocation.
A real X server integration run must additionally demonstrate:

1. Choose an opaque test window with a known pattern. Cover it with a different
   colored/textured window and confirm capture still contains only its content.
2. Destroy the selected window while the host waits for a connection; then create
   a replacement. Connecting must fail rather than show any replacement pixels.
3. During streaming, destroy/unmap/reparent/resize the selected window. The session
   must stop without publishing root pixels or reopening the XID.
4. Place the selected client inside a parent window. Between frame checks,
   unmap and immediately remap the parent without unmapping the client itself.
   Confirm the session terminates even though the client is VIEWABLE again.
   Repeat for a grandparent, including while waiting for the first connection.
5. Verify that `--allow-input`, `--allow-clipboard`, additional source grants and
   omission of `--once` all fail, including calls that bypass the CLI.
6. Verify that missing Composite, unsupported visuals and failed GetImage do not
   activate desktop capture. Confirm no global cursor or clipboard is sent.

A synthetic/Xvfb run is not RTX 5080 compositor, desktop, physical latency or
real dual-machine validation. Record those separately rather than treating the
new backend or telemetry as proof of competitive latency.

The dedicated-server regression suite is opt-in and never falls back to the
operator's `DISPLAY`:

```sh
# Start an empty, private 24-bit Xvfb with TCP disabled first.
JWM_REMOTE_TEST_DISPLAY=:99 cargo test --locked --no-default-features \
  --features remote-x11 --lib window_capture_integration -- --ignored --test-threads=1
```

The fixture refuses an existing compositor or application windows. It creates
its own Composite redirection and checks pixels, exact XID reuse, ancestor
unmap/remap and resize/restore against the actual server. Merely compiling these
ignored tests does not mean they passed. Environments that prohibit local X11
sockets cannot run this suite; do not weaken host security settings to do so.
