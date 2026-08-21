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
increasing record sequence numbers, and ChaCha20-Poly1305 on every
frame/input message.
The key is never sent over the network. Key files must be owned by the current
user, must not be symlinks, and must have no group/other permission bits.
Both peers enforce a total negotiation deadline, so slowly dripping handshake
or capability bytes cannot hold a connection open indefinitely. The host also
gives each complete authenticated video record, including its final flush, one
10-second write budget. Reading a few bytes at a time cannot restart that
budget; a partial or timed-out record closes the whole session so its wire
sequence can never be retried out of sync.

The current application protocol is version 4. Versions 2 and 3 are
deliberately incompatible, so update `jwm-remote` on both machines together;
negotiation rejects an older peer before screen or input data is exchanged.
Version 4 replaces whole-frame JPEG video with dirty-tile delta coding on the
same frame message; a version-3 peer would misread the first frame body, so the
handshake fails closed instead. Version 4
retains acknowledgements only for frames successfully drawn by the viewer,
which bounds host work and end-to-end video backlog. It carries input in
authenticated batches of 1–128 operations with a separate 641-byte payload
limit. The client coalesces only adjacent pointer positions (latest wins); a
key, button or release-all edge always ends that pointer run. Before causing
any input side effect, the host decodes and validates the complete batch,
preflights negotiated capabilities, the keyboard mapping and every XTEST
operation, then queues the batch in order and flushes XTEST once.
Authenticated record storage is reused only inside its owning network thread.
The receiver validates the declared length limit before growing that storage
and clears it on every truncated, malformed, or unauthenticated record; message
contents and kinds reach callers only after the tag verifies. The host declares
a 1 KiB inbound ceiling rather than the global 32 MiB frame limit, because it
only ever receives a hello, empty heartbeats, eight-byte acknowledgements and
input batches — an unauthenticated length field cannot make it reserve
megabytes.

### Transport encryption

Screen images and input are encrypted, not merely authenticated. This matters
most for input: a key press is a three-byte record, so a passive listener could
otherwise reconstruct a typed password, SSH passphrase or 2FA code byte for
byte with no image analysis at all.

The transport handshake is:

```text
server -> client: "JWMRT" || version (u16) || server_nonce (32)
client -> server: "JWMRT" || version (u16) || client_nonce (32) || proof (32)
server -> client: server_proof (32)
```

Both proofs and both traffic keys are derived over the complete transcript,
including each side's advertised version, so rewriting the version changes
every derived secret and the handshake fails rather than quietly settling on
weaker terms. The current transport version is 2; version 1 was the
authenticated-but-unencrypted transport, and its key-derivation domains are
separately versioned so the two can never collide even for an identical key.

Each record is `kind || sequence || ciphertext_len || ciphertext || tag(16)`.
The plaintext is sealed with ChaCha20-Poly1305 under a nonce of four zero bytes
followed by the record's big-endian sequence, with the 13-byte header
authenticated as associated data. Sequence numbers start at zero, advance by
exactly one, and exhausting the range is a hard error on both the read and the
write path, so a key/nonce pair is never reused. The 16-byte tag is *smaller*
than the 32-byte HMAC it replaces, and the whole record is assembled in one
reusable buffer and written with a single `write_all` — both peers set
`TCP_NODELAY` and neither wraps a buffered writer, so this replaced three
unbuffered socket writes per record with one.

Small records are padded to 64, 256 or 704 bytes before sealing, because length
alone otherwise distinguishes a keystroke batch from a pointer run from a
release-all. Larger records are not padded: a screen frame's size is dominated
by its content and no bucket could hide that. **Inter-keystroke timing remains
a side channel** — padding does not hide when you type, only what you type.

Measured cost on a 3440x1440 native-resolution session, the largest payloads
this carries: the telemetry `write` stage stayed at 0.0-0.1 ms, unchanged from
the unencrypted transport. ChaCha20-Poly1305 is pure Rust here and adds no C
toolchain or system dependency to the helper.

What this does **not** provide is forward secrecy: the traffic keys are a
function of the pre-shared key and both nonces, so anyone who later obtains the
key file can decrypt a recorded session. Rotate the key file if that matters,
and keep using SSH when you want an independently keyed channel:

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

The LAN defaults are a 12 FPS upper limit, adaptive JPEG quality 40–70, and a
maximum encoded width of 1280 pixels. They favor a reliable first connection
over maximum fidelity:

```bash
jwm-remote host ... --fps 24 --jpeg-quality 80 --max-width 1920
```

`--jpeg-quality` is the adaptive upper bound. Repeated same-setting pressure
from display-ACK latency, viewer superseding or exhausted two-frame credit
causes a multiplicative decrease; isolated slow ACKs are absorbed by a small
hysteresis score. Recovery is additive (`+1`) only after at least three seconds
and an FPS-scaled run of healthy ACKs. `--jpeg-quality-floor` changes the
default floor of 40; when the upper bound is below 40, the implicit floor
follows that upper bound for backwards compatibility. Use
`--fixed-jpeg-quality` to disable adaptation and always use the requested
quality (it conflicts with an explicit floor). Encoder quality changes do not
change the remote wire protocol.

`--max-width 0` keeps the native root width. Higher resolution, quality, and
frame rate increase CPU and bandwidth together. With XRender 0.10+,
`--max-width` also limits the X11 readback size for both overlay and root
capture. Root mode uses an `IncludeInferiors` graphics context to copy the root
and its same-depth children into a full-size staging pixmap, and creates a
Picture only for that pixmap, never for the root Window. It then scales into the
small target used by MIT-SHM or core readback. Core X11 does not guarantee the
result for inferiors at a different depth; a rejected request uses the CPU
path. The staging pixmap is limited to 64 MiB,
including the peak during geometry replacement; an oversized root uses the
existing full-size readback plus CPU resize. The accelerated path prints its
active source/output dimensions. MIT-SHM 1.2 file-descriptor
segments avoid copying image payloads through the X11 socket when the local X
server and transport support them; setup or runtime failure falls back to core
`GetImage` on the same frame and drawable. Standard depth-24, 32-bpp
little-endian TrueColor images are converted directly from native BGRX rows to
owned RGB storage; the host validates the current visual masks, byte order and
row layout before using that path, while all other formats keep the generic
pixel decoder. Capture and JPEG/network sending run as a two-stage pipeline
with one queued latest frame. The host permits at most two frames beyond the
latest one actually drawn by the viewer; while that credit is exhausted,
redundant X11 readback is automatically reduced to a periodic 250–1000 ms
refresh. An empty queue resumes the requested rate on its next tick.

The host caches the root geometry, compositor owner and XFixes cursor shape.
Checked core/RandR and XFixes notifications invalidate those caches, after
which the host performs one authoritative `GetGeometry`, `GetSelectionOwner` or
`GetCursorImage` query. A stable frame therefore fetches only the lightweight
pointer position and reuses the premultiplied cursor pixels, including their
scaled form. A cursor serial change during readback suppresses the stale cursor
for that frame and refreshes it on the next frame. Every source mode tracks
compositor-manager owner changes so a newly started compositor receives a fresh
capture-inhibitor property notification; event-driven stable frames do not poll
that selection, and Root mode never acquires the overlay. If an individual
notification facility is unavailable, only that cache returns to its reliable
polling path. RandR resize or compositor-owner races are reconciled and retried
once before XRender is disabled or Auto capture falls back to root; a transient
overlay-acquire failure is retried with a bounded delay. A staged root capture
also records the geometry epoch before `CopyArea` and validates it after the
small readback drains notifications. One changed epoch retries from an
authoritative geometry; repeated churn uses CPU for that frame. Polling mode
performs the same post-readback check with `GetGeometry`.

When the Composite overlay and XDamage are available, the host also avoids a
full drawable readback on a completely unchanged tick. It captures the first
frame and then whenever the active overlay reports Damage, the compositor owner
or root geometry changes, the cursor shape or pointer position changes, or a
two-second forced-refresh deadline expires. A Damage subtract request is queued
only after a tick commits to capture and before the synchronous drawable
readback on the same connection. That reply is the ordering barrier; an
asynchronous Damage rejection is drained before publication and permanently
disables Damage gating. A notification arriving during readback remains dirty
for the next tick, and failed readback never advances the deadline. The cursor
baseline records the position actually composited into the successful frame,
not an earlier gate probe. Root capture never negotiates XDamage. If the
extension is absent or a Damage request is rejected, overlay capture
permanently returns to the existing per-tick path for that session; X11
transport failures still end the broken session instead of pretending to fall
back.

The host emits rolling pipeline telemetry every five seconds, including an
explicit zero-send window while the sender is blocked on display credit or a
socket write. The viewer emits each non-empty five-second activity window. Both
peers emit one final non-empty partial snapshot during cleanup. Host counters
cover scheduled, captured, skipped, `damage-skipped`, published, dequeued,
encoded and sent work,
payload bytes, current/maximum outstanding credit and maximum queue age; its
stage averages cover capture, queue, credit wait, encode and write time.
`skipped` is capture-mailbox backpressure, while `damage-skipped` means XDamage,
geometry and cursor state proved that no readback was needed; those ticks do not
re-submit an old image to unchanged-frame comparison. Host
`replaced` means that a newer captured frame overwrote the one-slot
capture-to-sender mailbox before encoding. Viewer `replaced` instead means that
a newer decoded frame overwrote the viewer's one-slot latest-frame queue before
drawing; the viewer also reports received, decoded, drawn and ACKed counts plus
separate decode, queue and draw time.

A cumulative ACK for sequence B proves only that B was drawn. The host therefore
reports one `drawn-acks` target, all credits through B as `retired`, and the
earlier retired targets without an individual ACK as `viewer-superseded`.
`capture-to-ACK` and `send-to-ACK` end when the host receives that ACK, so they
include viewer queue/draw work, ACK creation and flush, and return-network delay;
they are not the viewer's draw-call duration. `drawn-bytes` counts only the JPEG
payloads of those proven ACK targets. `credit-wait` separately measures how long
the sender waited for an available display credit.

Adaptive quality attaches the exact encoder quality and a local setting epoch
to each in-flight credit. Only ACKs for the current epoch affect its pressure
score, and cumulative superseding counts only earlier frames from that same
epoch; late ACKs from the previous quality therefore cannot cause a second
decrease. Payload size remains telemetry and diagnostic context rather than a
discrete quality threshold, avoiding oscillation when JPEG output crosses an
arbitrary byte boundary. The ACK thread queues only bounded scalar feedback;
the video sender alone updates and reads quality immediately before encoding,
with encoding, logging and frame destruction outside the controller lock.

### Dirty-tile delta coding

Video is coded as 16-pixel tiles rather than whole frames. Before requesting a
quality setting or encoding, the sender compares the dequeued capture against
the pixels the viewer was last *sent* and collects the tiles that differ by
more than a small per-channel tolerance. Only those tiles are copied into one
packed atlas image, and that single atlas is JPEG-encoded.

The reference is deliberately the last transmitted content rather than the
previous capture. A region drifting slowly therefore still crosses the
tolerance against its own stale copy and is retransmitted, so the tolerance
bounds the error instead of letting it accumulate. It also makes committing a
frame proportional to the dirty area rather than the whole image.

One atlas beats one JPEG per dirty rectangle by roughly 3x on real desktops,
because a few hundred small rectangles otherwise pay a few hundred JPEG headers
and lose all shared Huffman statistics. Sixteen-pixel tiles keep every tile
aligned to a 4:2:0 minimum coded unit, so atlas neighbours cannot bleed chroma
into each other.

A frame with no dirty tiles is not sent at all, which subsumes the previous
exact-duplicate suppressor. At the four-second keepalive boundary an empty tile
frame — a header and a bitmap, no atlas — is sent instead, keeping static
sessions inside the viewer's shared eight-second video idle timeout. The
conservative timing budget is four seconds of keepalive plus the two-second
forced XDamage refresh and one second of scheduling margin, still strictly
below that eight-second timeout.

The first frame of a session, a root geometry change and an encoded-size change
each force a keyframe carrying every tile. A failed write or flush never
advances the reference, so the host's model of the viewer's canvas cannot run
ahead of what the viewer actually received.

Measured on a 3440x1440 JWM session sharing a lightly-active desktop over
loopback, against the previous whole-frame encoder:

| | whole-frame | dirty-tile | |
|---|---|---|---|
| wire, `--max-width 1280` | 10.1 Mbit/s | 0.5-0.9 Mbit/s | ~15x |
| wire, `--max-width 0` | 53.3 Mbit/s | 1.8-2.6 Mbit/s | ~25x |
| encode, 1280 wide | 9-17 ms | 0.4-1.2 ms | ~20x |
| encode, native | 60 ms | 1.5-2.7 ms | ~30x |
| capture-to-ACK, native | 80-89 ms | 12-14 ms | ~6x |

The 60 ms native encode had capped the achievable rate near 16 fps regardless
of link speed; that ceiling is gone.

Host telemetry reports suppressed frames as `unchanged-suppressed` and periodic
empty frames as `unchanged-keepalive`; both are subsets of `dequeued`, while
only the latter is also `encoded` and `sent`. `keyframes` counts frames
carrying every tile, and `tiles A/B (P% dirty)` reports transmitted versus
total tiles for the window — the single best indicator of how much the codec is
actually saving on the content being shared.

The sender writes each JPEG directly behind its frame header in one reusable
allocation while hard-bounding the total payload length, and the viewer's
receiver likewise reuses one authenticated record allocation. After 32
substantially smaller payloads, an allocation retained by an exceptional frame
shrinks toward an 8 MiB ceiling; normal steady frame sizes do not churn
allocations.
After authentication and header validation, an ordinary RGB JPEG is decoded
directly into a reusable RGB allocation. The allocation stays leased while the
frame is queued or retained by the X11 viewer for Expose and resize, then
returns only after replacement, draw failure, or window close; two concurrently
live frames therefore never alias. The best-fit free list keeps at most two
buffers, rejects any slot above 32 MiB, and retains at most 64 MiB total.
Uncommon grayscale JPEGs retain the compatible bounded conversion path.
The X11 viewer reuses its native upload allocation for unchanged display
geometry. On the usual depth-24, 32-bits-per-pixel little-endian TrueColor
visual it writes decoded RGB directly into the verified native layout and uses
an MIT-SHM 1.2 file-descriptor segment to avoid sending that image through the
X11 socket. The segment is reused only after the matching server completion
event; setup or rejected-upload failures retry the same frame with core
`PutImage`. Nonstandard visuals use the general mask-based conversion and core
upload. Expose events copy the last completely uploaded backing image, and
resize event bursts redraw only their final size.
When a large viewer window displays the encoded image at more than its native
width and height, XRender 0.10+ keeps the uploaded image in a source-sized
server pixmap and scales it into the letterboxed backing pixmap. The client no
longer constructs or uploads a window-sized image on every new frame, so a
bandwidth-friendly host `--max-width` can still be viewed fullscreen without
moving the corresponding upscale work through CPU memory. This changes only
local presentation: it does not increase the encoded resolution or network
traffic. One-to-one presentation and downscaling keep the existing CPU path;
an unavailable or unusable XRender path falls back there as well.
When idle, the viewer blocks on its X11 connection, the video receiver's wake
descriptor, and the nearest heartbeat, telemetry or deferred-key deadline; it
also checks x11rb's internal and MIT-SHM-deferred event queues before sleeping.
With input negotiated, closing the viewer flushes the close-time `ReleaseAll`
batch before sending one authenticated `Close`; view-only close sends only the
`Close`. If X11 drawing or network I/O has already failed, the client instead
shuts the session transport down immediately and lets the host's disconnect
cleanup release injected input. Cancellation and shutdown are idempotent, and
the blocking receiver thread gets a bounded join window.
Root capture queues its `IncludeInferiors` `CopyArea` and XRender Composite
before the synchronous small-target readback, then checks both requests against
that ordering barrier. A rejected request cannot publish an older target image.
Older or unusable XRender servers, staging roots above 64 MiB, and per-frame
staging failures retain the full-resolution readback plus CPU-resize fallback;
for those paths, lower `--fps` as well as `--max-width` on very large combined
multi-monitor roots.
Every frame write has a 10-second absolute deadline across its header, JPEG,
authenticator, and flush. If it expires, the host tears down that session and
the normal cleanup path releases any keys or buttons held by its controller.

### Session resilience

The listener survives everything that concerns a single connection. An
unauthenticated peer that connects and immediately resets, an aborted
connection, an interrupted syscall, or descriptor exhaustion are all logged and
retried rather than ending the host; only a genuinely broken listener stops it.
The peer address comes from `accept` itself, because asking the accepted socket
for it is a liveness question that a reset peer answers with an error.

`--once` returns the session's own result, so a scripted run that failed is
distinguishable from a clean one by exit code.

While the controller holds any key or button, the host requires it to stay
audible: 600 ms of silence releases everything held, without ending the
session. The host X server generates autorepeat, so without this a network
partition with a key down kept typing into whatever had focus for the full
eight-second idle timeout. The host waits for the socket to become readable
rather than shortening its read timeout, so a record is never interrupted
part-way through.

A button the peer sends that this host's pointer map does not define — a
12-button mouse, or horizontal-scroll buttons 6 and 7 — is dropped rather than
failing the batch. Because a batch is validated before anything is queued,
failing it also discarded every pointer motion and any release-all travelling
alongside. The physical button is pinned when a press is queued, so a pointer
remap arriving mid-press cannot release a different button and leave the real
one stuck down.

The client bounds its TCP connect. Every later phase already had an absolute
deadline, but a black-holed address otherwise burned the kernel's full SYN
retry budget against a five-second negotiation budget.

An overlay readback failure now arms the same bounded retry as an overlay
acquire failure. Without it, re-acquisition waited for a compositor-owner
transition that never arrives while the same compositor keeps running, so one
transient error downgraded the session to ungated root capture — and with it
the XDamage gate — for the rest of the session.

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
- JPEG tile video only: no audio, clipboard, file transfer, or inter-frame
  codec yet;
- video uses cumulative display acknowledgements, at most two unacknowledged
  frames, and one latest queued frame; persistently slow viewers therefore
  reduce capture/encode/network work instead of accumulating stale video;
- encrypted and authenticated direct-LAN transport, without forward secrecy:
  a leaked key file decrypts previously recorded sessions;
- inter-keystroke timing is not hidden;
- X11 `x11rb`/`xcb` JWM sessions only.

These boundaries keep the first end-to-end path small and observable. A future
version can replace JPEG/TCP with a hardware video path and encrypted transport
without exposing JWM's local IPC or moving network waits into the compositor.
