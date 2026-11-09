# Explicit source-size following (direct X11 viewer)

`--follow-source-size` is **off by default**. It is not local image scaling:
resizing the viewer requests changes to the **real remote application's window
size**. Other people using that remote desktop can see its layout change.
Closing the viewer or stopping follow does not restore the old remote size.

Enable it only when that is the intended behavior:

```sh
# Host: explicitly authorize one locally selected X11 window instance.
# Keep loopback binding unless you separately intend and authorize LAN access.
jwm-remote host --window 0xYOUR_WINDOW --once --allow-window-resize \
  --listen 127.0.0.1:48221 --key-file /private/existing.key

# Viewer: use an already-authorized reachable endpoint (for example an existing
# manually configured tunnel to that loopback listener).
jwm-remote connect ENDPOINT --viewer-backend x11 --view-only \
  --follow-source-size --key-file /private/existing.key
```

`0xYOUR_WINDOW` is a placeholder, not a valid XID. The host's existing native
resize authority is still required. This option does not grant a new target,
open a listener, establish a tunnel, choose another window, override a native
resize refusal, or enable input/clipboard access. Normal tiling, visibility,
fullscreen/maximize, locking, window-instance and size-hint checks still apply.
A single-window resize grant is not permission to resize the entire desktop.

## Deliberately narrow first version

- Direct `connect` only, using the local **X11** viewer and explicit `--view-only`.
- Not available through the `ssh` helper or the native Wayland viewer.
- Incompatible with one-shot `--resize-source`, keyboard/pointer leases,
  clipboard, input grabs and automatic reconnect. Even explicitly supplying
  `--reconnect-attempts 0` conflicts with the follow flag; omit the option.
- Not saved in a profile. Profile save rejects it rather than silently dropping
  consent; hand-written unknown profile fields are rejected too. Existing
  profiles and SSH client construction always keep following disabled.
- Local `--window-size` remains a local initial window-size request. Only the
  separate explicit follow flag allows subsequent source-size requests.

## Runtime behavior and stop boundary

After the first real presentation, the client compares the actual viewer client
area with the source size. Equal sizes need no request. Otherwise it follows
actual viewer client-area changes, coalescing resize bursts
rather than queuing an unbounded history. Requests remain source/geometry-scoped,
with at most one outstanding native resize and no more than four requests per
second. The next request waits at least 250 ms after both the previous result
and its matching actual presentation have completed. A new source
frame/presentation must establish the required valid scope;
local window changes do not themselves prove that the remote resize applied.

Native `Configured` is not completion. The existing resize protocol only reports
`Applied` after capture observes the corresponding actual geometry. Window-manager
size hints can constrain a request, and the remote application may reject it.
The client confirms completion only after the matching actual dimensions and
observed geometry epoch are presented. An `Applied` size constrained by hints
counts as served for that viewer-size revision: it is not retried to fight the
window manager. Only a new viewer-size revision can request another resize.

Any non-`Applied` result (including refusal, `Unchanged`, stale/busy/failure),
a five-second result/presentation timeout, or a selected-source change **ends
following for that connection**. There is no automatic retry/re-arm loop. If the connection and
source remain healthy, the viewer continues receiving read-only frames. The
already-authorized host's lifecycle policy can still end capture/session itself
on unmap, lock, destruction or disconnect; this option cannot promise to keep a
revoked source visible. Starting a new explicit session is required to opt in
again. Already applied remote geometry is left in place.

If the host does not advertise the required resize grant, startup fails explicitly
instead of pretending following was enabled. Logs distinguish enabled, requested,
confirmed `Applied`, and stopped states; a sent request is not a confirmed resize.

This is a convenience policy over the existing native resize capability, not
new remote-input authority. It does not send XTEST events, manipulate focus,
move the remote window, persist a resize permission or broaden host access.
