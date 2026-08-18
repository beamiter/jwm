# JWM-to-JWM remote control (X11 LAN MVP)

`jwm-remote` shares one JWM X11 desktop with another JWM X11 session. It is
implemented once for the X Composite surface used by both the `x11rb` and
`xcb` backends. Windows/macOS, Wayland sessions, and other Linux window
managers are outside this first compatibility target.

The helper is a separate process: X11 readback, JPEG encoding, a slow peer,
and a broken network stay outside JWM's compositor event loop. With
XRender 0.10+ and the Composite overlay, the host downsizes the final desktop
in the X server and reads back only the encoded dimensions. This keeps large
roots from dominating the capture path. The host includes JWM effects and
system UI, while an independent XTEST connection injects authenticated input.

Managed release bundles keep the helper in their versioned `current` tree.
Add that directory before using the commands below (source installs already
place the helper in `/usr/local/bin`):

```bash
export PATH="/usr/local/lib/jwm/current/bin:$PATH"
```

## Security boundary

Remote control is disabled until `jwm-remote host` is started. The host listens
only on `127.0.0.1` by default. A non-loopback listener requires the explicit
`--allow-lan` flag, and input requires the separate `--allow-input` flag.

Every session uses a 256-bit pre-shared key, fresh client/server nonces,
role-separated HMAC-SHA256 proofs, independent traffic keys, strictly
increasing record sequence numbers, and a MAC on every frame/input message.
The key is never sent over the network. Key files must be owned by the current
user, must not be symlinks, and must have no group/other permission bits.
Both peers enforce a total negotiation deadline, so slowly dripping handshake
or capability bytes cannot hold a connection open indefinitely. The host also
gives each complete authenticated video record, including its final flush, one
10-second write budget. Reading a few bytes at a time cannot restart that
budget; a partial or timed-out record closes the whole session so its wire
sequence can never be retried out of sync.

The current application protocol is version 2. Update `jwm-remote` on both
machines together; negotiation rejects older peers before screen or input data
is exchanged. Version 2 acknowledges only frames successfully drawn by the
viewer, which bounds host work and end-to-end video backlog.

The LAN MVP authenticates traffic and rejects modification/replay, but **does
not encrypt screen images or input**. Use it only on a trusted, isolated LAN.
For confidentiality, keep the default loopback listener and carry it over SSH:

```bash
# Host
jwm-remote host --key-file ~/.config/jwm/remote.key --allow-input

# Client (keep this running), then connect to 127.0.0.1 below
ssh -N -L 48221:127.0.0.1:48221 user@192.168.1.50

# Client (in a second shell)
jwm-remote connect 127.0.0.1:48221 \
  --key-file ~/.config/jwm/remote.key --grab-input
```

## Build

The default build includes `jwm-remote`:

```bash
cargo build --locked --release --bin jwm-remote
```

The source installer places it in `/usr/local/bin`. A managed release bundle
keeps the new helper at `/usr/local/lib/jwm/current/bin/jwm-remote` so adding it
does not break upgrades from older release manifests. When switching from a
source install to a managed bundle, run `type -a jwm-remote`: an old
`/usr/local/bin/jwm-remote` is intentionally not taken over by the managed
installer and should be removed only after verifying the managed command above.

Slim X11 builds must keep the `remote-x11` feature. The helper still works
with either selected JWM backend because it opens its own X11 connection:

```bash
cargo build --locked --release --no-default-features \
  --features backend-x11rb,remote-x11 --bin jwm --bin jwm-remote

cargo build --locked --release --no-default-features \
  --features backend-xcb,remote-x11 --bin jwm --bin jwm-remote
```

Both machines need an X11 JWM session. The host X server needs Composite;
interactive control additionally needs XTEST. The two machines should use the
same XKB layout for keyboard control because the MVP forwards X11 keycodes
(physical keys), not text. The handshake fingerprints both keymaps; a mismatch
fails closed by disabling keyboard injection while leaving pointer control on.

## First LAN connection

Create the key once on either machine. The command refuses to overwrite an
existing path:

```bash
install -d -m 700 ~/.config/jwm
jwm-remote keygen --output ~/.config/jwm/remote.key
```

Copy that file to the other machine through a secure channel, then restore its
private mode:

```bash
ssh user@192.168.1.60 'install -d -m 700 ~/.config/jwm'
scp ~/.config/jwm/remote.key user@192.168.1.60:~/.config/jwm/remote.key
ssh user@192.168.1.60 'chmod 600 ~/.config/jwm/remote.key'
```

On the machine being controlled:

```bash
jwm-remote host \
  --listen 0.0.0.0:48221 \
  --allow-lan \
  --allow-input \
  --key-file ~/.config/jwm/remote.key
```

Restrict TCP port `48221` to the intended local subnet in the host firewall.
On the controlling JWM machine, replace the address with the host's LAN IP:

```bash
jwm-remote connect 192.168.1.50:48221 \
  --key-file ~/.config/jwm/remote.key \
  --grab-input
```

With `--grab-input`, click inside the viewer before using JWM shortcuts. Press
F12 to release the local keyboard/pointer grab and release every remotely held
key/button. The client refuses `--grab-input` if its base keymap has no plain
F12 escape key. Closing the viewer also performs that release. Without
`--grab-input`, ordinary pointer and key events are forwarded while the viewer
has focus, but local JWM global shortcuts may win their passive grabs.

For view-only use, omit host `--allow-input` or pass client `--view-only`:

```bash
jwm-remote connect 192.168.1.50:48221 \
  --key-file ~/.config/jwm/remote.key \
  --view-only
```

## Tuning and troubleshooting

The LAN defaults are a 12 FPS upper limit, JPEG quality 70, and a maximum
encoded width of 1280 pixels. They favor a reliable first connection over
maximum fidelity:

```bash
jwm-remote host ... --fps 24 --jpeg-quality 80 --max-width 1920
```

`--max-width 0` keeps the native root width. Higher resolution, quality, and
frame rate increase CPU and bandwidth together. With overlay capture and
XRender 0.10+, `--max-width` also limits the X11 readback size. The accelerated
path prints its active source/output dimensions. MIT-SHM 1.2 file-descriptor
segments avoid copying image payloads through the X11 socket when the local X
server and transport support them; setup or runtime failure falls back to core
`GetImage` on the same frame and drawable. Long-running sessions report rolling
capture/queue/ack/encode/write latency plus captured, skipped, replaced, and
outstanding frame counts. Capture and JPEG/network sending run as a two-stage
pipeline with one queued latest frame. The host permits at most two frames
beyond the latest one actually drawn by the viewer; while that credit is
exhausted, redundant X11 readback is automatically reduced to a periodic
250–1000 ms refresh. An empty queue resumes the requested rate on its next tick.
Root compatibility capture and older XRender servers retain the
full-resolution readback plus CPU-resize fallback; for those paths, lower
`--fps` as well as `--max-width` on very large combined multi-monitor roots.
Every frame write has a 10-second absolute deadline across its header, JPEG,
authenticator, and flush. If it expires, the host tears down that session and
the normal cleanup path releases any keys or buttons held by its controller.

- A black/invalid Composite overlay can be bypassed with
  `--capture-source root`. Root capture is a compatibility fallback and may
  omit compositor-only effects on some X servers.
- `--capture-source overlay` fails instead of falling back, which is useful
  while diagnosing Composite readback.
- “permission denied” for a key is normally fixed with `chmod 600 KEYFILE`;
  symlink key paths are intentionally rejected.
- “connection refused” means the host is not listening on that address/port or
  a firewall blocks it. Check the host's printed listener address first.
- If letters differ between machines, configure matching XKB layouts. Pointer
  and button control are independent of the keyboard layout.

## MVP limits

- one connected controller at a time;
- the combined X root is shared (all monitors), with the host XFixes cursor
  included in video; the viewer cursor is transparent during ordinary input
  forwarding and an active grab, while view-only and released-grab windows keep
  a visible local cursor; the rest of the client desktop is never affected;
- JPEG video only: no audio, clipboard, file transfer, or adaptive codec yet;
- video uses cumulative display acknowledgements, at most two unacknowledged
  frames, and one latest queued frame; persistently slow viewers therefore
  reduce capture/encode/network work instead of accumulating stale video;
- authenticated but unencrypted direct-LAN transport;
- X11 `x11rb`/`xcb` JWM sessions only.

These boundaries keep the first end-to-end path small and observable. A future
version can replace JPEG/TCP with a hardware video path and encrypted transport
without exposing JWM's local IPC or moving network waits into the compositor.
