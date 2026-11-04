# Desktop Visual Records, version 1 (DVR/1)

DVR is a language-neutral, local, **data-only** extension boundary. A producer
renders in its own process and streams pixels; the desktop consumer renders them
as an overlay. No producer library, shader, script, command, or geometry callback
is loaded into the compositor. Connecting grants **neither input events/control
nor screen capture**. A consumer must explicitly enable its endpoint.

This crate is independent of WaterLily, JWM IPC and any GPU API. WaterLily's older
`JWMLILY` file/notification protocol and its volume/material/jelly rendering
remain a separate compatibility adapter; those effect-specific records are not
DVR capabilities and cannot be sent to a DVR endpoint.

## Endpoint and lifetime

- Linux Unix stream socket in a same-effective-UID private directory (mode 0700).
  The server creates a missing final directory with 0700, not arbitrary parent
  directory chains; it rejects a symlink final parent and non-private parents.
- Socket mode is 0600. `SO_PEERCRED` must identify the same effective UID.
- One producer connection is served at a time. An active socket is never
  replaced. Stale reclamation requires an immediate `ECONNREFUSED` from a
  nonblocking probe, same-user socket metadata and unchanged device/inode.
- Server cleanup removes only the socket inode it created. The producer owns
  its process, socket connection and local frames; there are no shared files.
- Any bad record, allocation failure, EOF, timeout or resize closes that
  connection, discards its pending frame and clears connected state. A producer
  may reconnect with capped retry delay; every connection starts fresh.
- Each authenticated accept advances a consumer-local connection epoch, even
  if the output size is unchanged. Retained textures must be tagged with the
  publishing frame's epoch and hidden when it differs from the server epoch.
  Checking connected state alone is insufficient: disconnect and reconnect may
  both occur between render polls. Epochs are never producer-controlled or sent
  on the wire; exhaustion closes the endpoint rather than reusing an identity.
- On shutdown, the server sets a stop flag and joins its worker. Accept polls
  at 10 ms and socket waits at 20 ms. It never connects to a pathname to wake a
  thread and never sends a process signal.

## Framing and capabilities

All integers are unsigned **little-endian**. All headers are exactly 64 bytes.
There is no JSON, delimiter scanning, fd passing or optional trailing extension.
The server first sends one `Hello`; producers must validate it before writing
frames. The only subsequent records are `Frame header || stride*height bytes`
in the producer-to-server direction. Unknown versions, formats, alpha modes,
origins or nonzero reserved fields are rejected before payload allocation.

### Server Hello

| Offset | Bytes | Value |
|---:|---:|---|
| 0 | 8 | ASCII `DVRHELLO` |
| 8 | 4 | Protocol version: `1` |
| 12 | 4 | Header bytes: `64` |
| 16 | 4 | Complete output canvas width, physical pixels, 1..65535 |
| 20 | 4 | Complete output canvas height, physical pixels, 1..65535 |
| 24 | 4 | Maximum frame width or height: `4096` |
| 28 | 4 | Maximum padded frame payload bytes: `33554432` (32 MiB) |
| 32 | 4 | Pixel-format capability mask: `1` (RGBA8 only) |
| 36 | 4 | Alpha capability mask: `7` (bits 0,1,2 = opaque, straight, premultiplied) |
| 40 | 4 | Origin: `1` (top left) |
| 44 | 20 | Reserved, zero |

The current implementation advertises this fixed v1 capability set; additions
require a new protocol version, not reinterpretation of reserved fields.

### Frame

| Offset | Bytes | Value |
|---:|---:|---|
| 0 | 8 | ASCII `DVRFRAME` |
| 8 | 4 | Protocol version: `1` |
| 12 | 4 | Header bytes: `64` |
| 16 | 4 | Width, 1..4096 |
| 20 | 4 | Height, 1..4096 |
| 24 | 4 | Row stride in bytes, at least width*4 |
| 28 | 4 | Pixel format: `1` (R,G,B,A bytes in that order) |
| 32 | 4 | Color space: `1` (sRGB-encoded RGB) |
| 36 | 4 | Alpha: `1` opaque, `2` straight, `3` premultiplied |
| 40 | 4 | Origin: `1` (top left, rows run down) |
| 44 | 4 | Reserved, zero |
| 48 | 8 | Sequence, nonzero and strictly increasing per connection |
| 56 | 8 | Producer monotonic timestamp in nanoseconds (informational) |

`stride * height` is checked using wide arithmetic and must not exceed 32 MiB.
The limit includes row padding. The receiver compacts padding on its worker
thread before publishing a tight `width * height * 4` RGBA buffer. No payload
pointer or file offset crosses the trust boundary. Sequence counters restart
only on a new connection. Timestamp is not synchronized to the compositor and
is not used as a deadline or permission decision.

Opaque mode ignores stored alpha and displays alpha 1. Straight-alpha RGB is
multiplied by A exactly once during rendering. Premultiplied RGB is already
multiplied in its sRGB-encoded domain; producers must keep RGB <= A, including
RGB=0 when A=0. Consumers may defensively clamp invalid premultiplied RGB. The
v1 source-over blend contract is display-referred, not a claim of linear-light
color accuracy or HDR support.

## Coordinates, outputs and resizing

A frame is a complete normalized image of the consumer's **whole output
canvas**, not a window or a monitor ID. Source pixel `(0,0)` is the top-left of
that canvas; its right/bottom edges map to the canvas's right/bottom edges.
Consumers scale the image to the physical canvas extent, including smaller
producer images. This permits a low-resolution producer while bounding memory.
DVR/1 exposes no window list, output identities, pointer coordinates or scale
factor beyond the canvas's physical dimensions. Per-output positioning,
independent per-monitor scale, damage rectangles and interaction are not v1
capabilities. JWM has X11 and native Wayland GLES full-canvas SDR consumers;
the Wayland candidate is offscreen-shader-tested, with physical-output acceptance
still pending. A transport or shader test alone does not establish actual
presentation, per-output routing or HDR support.

A changed canvas size invalidates old-generation frames immediately and closes
the current connection within the worker's bounded I/O poll. Reconnection gets
a fresh Hello. Producers must reread capabilities and rerender for the new
extent. Reapplying the same size leaves the connection intact. Zero extents
are invalid; a consumer should disable its endpoint if no canvas exists.

## Backpressure and nonblocking consumption

The server owns at most one in-progress payload and one newest completed frame.
Replacing the newest frame drops the older one: **latest wins**, no frame queue.
The compositor may additionally retain the one frame handed to it. Each buffer
is bounded by 32 MiB (padding-compacted Vec capacity may retain that bound).
Socket kernel buffers provide bounded transport backpressure; they are not an
unbounded user-space queue. Producers should cap their own queue to one frame
and use finite send deadlines. On a partial/failed send they must close the
connection rather than append a new header to a corrupted stream.

The compositor uses only atomics and `try_lock` for `has_pending`, `connected`,
`set_output_size` and `try_take_latest`. Contention means try again on a later
poll, never wait. All socket I/O, validation, allocation and compaction run on
the receiver thread. The installed waker must be a trusted nonblocking event
loop notification. `bind`/`Drop` are lifecycle work, not frame-loop operations.

Idle connections get 30 seconds to begin a frame. Once the first header byte
arrives, the **entire header plus payload** must finish within 2 seconds; small
periodic writes cannot reset that deadline. Complete valid frames reset the
idle window. A slow or crashed producer cannot block the compositor, and the
next producer can connect after cleanup. No acknowledgments are required: a
frame can be superseded before display, so successful sending is not a display
receipt.

## Tests

`cargo test --locked -p desktop_visual` covers parsing/limits and private-socket
lifecycle, malformed/truncated input, sequence rejection, latest-wins handoff,
slow/idle producer deadlines, reconnect, canvas resize, cleanup ownership,
start/stop and nonblocking render handoff. It requires permission to create
local Unix sockets, but no display, GPU, credentials or external service.

### Native Wayland selected-output endpoints

`JWM_DESKTOP_VISUAL_OUTPUTS` optionally maps exact native output names to private
socket paths, for example `{"DP-1":"/run/user/1000/jwm-desktop-visual/DP-1.sock"}`.
The existing `JWM_DESKTOP_VISUAL_ENABLED=1` switch and full-canvas endpoint remain
required. Each selected endpoint uses the unchanged v1 handshake and bounded
RGBA8/sRGB payload contract; its advertised extent and destination rectangle are
that output's portion of the compositor canvas. The endpoint path supplies the
output identity, so a producer never supplies or guesses desktop coordinates.
Up to four selected endpoints are active, with one latest frame per endpoint and
the existing 32 MiB per-frame limit. Unplug, destination movement, resize, or
configuration-path changes remove the old endpoint and its GPU texture. A fresh
connection is required even for a same-size replug. X11 and nested Wayland retain
the v1 full-canvas route; selected-output negotiation is native Wayland only.

### Companion presentation evidence

Companion retirement is separate from DVR ingress. Native Wayland attaches an
immutable companion frame ticket to the exact queued DRM framebuffer. Only a
matching timestamp-validated, non-ambiguous page-flip with returned queue data
confirms it. GLX uses the SBC returned by the actual OML swap and a subsequent
successful `glXGetSyncValuesOML` completed-SBC observation. A successful ordinary
swap, GL upload, queued submission, watchdog cancellation, DPMS-off cleanup, or
HDR property operation supplies no presentation evidence. EGL and nested routes
without such a completion source advertise no retirement capability.

Tickets retain their exact token, render-start time, and route generation.
`Presented` requires a successful companion draw on an intersecting output.
After a revoke, `Retired` waits for a body-free frame captured after that revoke
on every active output in the same route generation. Topology/power changes and
backend teardown invalidate old tickets. These are software/native-compositor
observations; they do not claim physical-panel certification.
