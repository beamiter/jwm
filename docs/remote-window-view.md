# X11 application-window viewing (experimental)

This is the first, deliberately **view-only** window-scoped slice of the remote
application plan. It offers optional explicit native resize, but does not implement exclusive input.
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
Destroy, unmap or reparent of the window **or any ancestor** revoke the grant. Ancestor lifecycle notifications are
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
size. Remote-requested native resizing, input ownership, focus/lock handling, reconnect recovery
and Wayland window grants remain separate follow-up work.

## Required validation before deployment

Automated policy/state tests cover invalid grants and terminal revocation.
A real X server integration run must additionally demonstrate:

1. Choose an opaque test window with a known pattern. Cover it with a different
   colored/textured window and confirm capture still contains only its content.
2. Destroy the selected window while the host waits for a connection; then create
   a replacement. Connecting must fail rather than show any replacement pixels.
3. During streaming, destroy/unmap/reparent the selected window. The session
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

## Local resize of an authorized window

A local user or application may resize the same live authorized window without
ending view-only capture. The capture backend stops producing frames from a
known-stale named pixmap, obtains a new one, and validates actual pixmap geometry
and lifecycle events before returning a frame.
Each accepted backing-store replacement advances a checked geometry epoch, even
when a resize is followed by restoration to the original dimensions. The existing
geometry-keyframe path supplies a fresh canvas to the viewer. A resize racing
capture drops that readback and retries on a later capture tick; it never falls
back to a root rectangle or returns a frame from a known-stale backing store.
This does not retract frames already captured, queued or sent; an older queued
frame may be delivered before the fresh geometry-epoch frame. Remote resize/input
will need a separate geometry publication fence.

This is **not** remote resize: the viewer still changes only encoded bounds.
There is no new input, clipboard, reconnect or resize RPC permission. Destroy,
unmap, reparent, compositor changes and errors still revoke permanently. A new
pixmap is obtained only while the original instance/ancestor watchers stay live.
Real-server tests cover changed dimensions, resize/restore, and 100 consecutive
resizes with alternating repainted colors followed by destruction/XID reuse.
Deterministic after-name hooks also test resize/restore candidate rejection and
destroy/exact-XID-reuse revocation before readback; those tests must run, not merely compile, before calling this validated.

### Lock-screen limitation

Named-pixmap capture intentionally continues while another window obscures the
application. There is currently **no trusted host lock-screen or focus policy**
in this backend. Do not assume locking the desktop automatically revokes sharing:
the local host must explicitly stop the session/grant. This is not safe exclusive
control or production-complete remote control. A trusted native JWM lock/lease
boundary is separate prerequisite work before adding remote input privileges.

## Explicit native source resize (experimental)

The host must opt in independently with `host --window XID --once --allow-window-resize`.
The viewer may issue one operation with `connect ADDRESS --view-only --resize-source 800x600`.
Keep the normal key-file arguments. Ordinary viewport resizing and `--window-size`
only adjust the viewer/encoded picture, never the application's native geometry.
No keyboard, pointer, clipboard, reconnect, focus switching or generic IPC is enabled.

Application protocol v15 adds a separate negotiated capability and fixed-size
28-byte request / 37-byte result, bound to source epoch, geometry epoch and
monotonically increasing request ID. The host admits one pending operation,
limits native execution to four requests per second, and disables resize after
an uncertain native failure or three-second confirmation deadline. `Configured`
from local IPC is not success: a fresh capture must confirm actual dimensions
and, for a changed size, a newer geometry epoch before `Applied` is returned.
`Unchanged` also requires native validation and a fresh capture. This is capture
confirmation, not confirmation that the viewer has presented the result.
Already queued or transmitted old video cannot be retracted.

A private persistent same-user local IPC connection binds a server-owned grant
to the selected ClientKey generation. A random 32-byte STRING property binds the
grant to the actual X server and target, not merely a matching numeric XID.
The property remains throughout the lease; native application rechecks it inside
a short X server critical section. Cleanup compares the nonce before deleting it.
The network cannot supply a native XID, nonce or arbitrary local command.

Native resize additionally requires an InputOutput client with zero border;
this does not promise support for arbitrary decorated windows.

The native backends invalidate the proof on raw Unmap/Reparent/Destroy events,
including events prefetched before normal dispatch. Grant/application fences
retain raw events in a shared ordered queue capped at 256; uncertainty or budget
exhaustion fails closed rather than discarding lifecycle history. Queue exhaustion,
raw read errors or failed proof invalidation permanently disable native resize
in that WM process, until JWM is restarted; ordinary window management continues.
This is not merely a per-viewer reset. Native size
hints are followed by limits of 16,384 pixels per axis and 67,108,864 total pixels.
JWM's native lock transitions revoke resize grants. They do **not** stop the
view-only named-pixmap capture or reliably detect third-party screen lockers.
Covered windows intentionally continue to be shared; stop the local host to end
that permission. This is not production-complete remote control or isolation
from other same-user X11 clients.

The reconstruction must pass fresh compile, unit and dedicated real-X CI tests;
results from an earlier lost cloud workspace are not validation of this tree.
