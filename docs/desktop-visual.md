# Desktop visual extensions (DVR v1)

DVR is a **data-only, out-of-process rendering boundary**, not a plugin loader.
A program in any language may connect to the opt-in endpoint, read its capability
hello, and publish ordinary RGBA8 images. JWM receives and validates frames on a
worker thread, keeps only the latest completed image, and draws an input-transparent
source-over overlay. Producers cannot supply shaders, compositor code, paths to
shared memory, input injection requests, or screen-capture requests.

## Ownership after the WaterLily extraction

| Component | Owner / location |
| --- | --- |
| Simulation, fourteen cases, palettes, CPU/GPU selection, Julia environment and tests | `beamiter/juliafun/waterlily` |
| Reusable Julia DVR producer SDK (no WaterLily dependency) | `beamiter/juliafun/desktop_visual` |
| Language-neutral protocol, bounded receiver and validation | `crates/desktop_visual` |
| Python SDK and unrelated plasma example | `extensions/desktop_visual` |
| Generic GL image consumer | `src/backend/x11/compositor/desktop_visual.rs` |
| Old shared-file decoder, optical/volume shaders and jelly pose renderer | JWM **legacy WaterLily consumer adapter**, retained for existing effects |
| Old shell commands | Compatibility launchers; locate the external Julia project |

There is no copy of the Julia solver or case implementation in JWM. The legacy
adapter is intentionally distinct: its `JWMLILY` payloads can represent optical
height, volume material or jelly poses, **not ordinary RGBA images**. Reinterpreting
those bytes as DVR would silently break rain, puddles and volumetric effects.
Existing WaterLily controls, frame/socket environment overrides and negotiated
versions 1–4 continue to work with the relocated provider. The provider's explicit
`--transport desktop-visual` mode uses the generic SDK for supported planar image
output; see its README for exclusions. The default remains legacy for compatibility.

## First run

DVR v1 is implemented for the shared **X11 compositor (`x11rb` and `xcb`)**. It is
not exposed by Wayland/DRM or nested Wayland yet; a successful transport unit test
is not evidence of Wayland or real-GPU support. The interface is backend-neutral,
but each backend must explicitly implement image presentation before advertising it.

Start a composited X11 JWM session with:

```sh
JWM_DESKTOP_VISUAL_ENABLED=1 jwm --backend x11rb
```

The endpoint is `$XDG_RUNTIME_DIR/jwm-desktop-visual/visual.sock`. No insecure `/tmp`
fallback is used. `JWM_DESKTOP_VISUAL_SOCKET` may specify another absolute path
whose immediate parent is a private same-user directory. JWM creates only the
last parent directory if absent, mode 0700, and the socket mode is 0600. The endpoint
is absent by default; startup bind/GL failure disables this extension, not JWM.

Run the non-WaterLily example in another terminal:

```sh
python3 extensions/desktop_visual/plasma.py
```

Ctrl+C stops the producer and removes the overlay; stopping JWM closes the socket.
There is no helper that kills unrelated processes. To disable the facility, restart
JWM without `JWM_DESKTOP_VISUAL_ENABLED`. Existing WaterLily toggle/case/palette
commands act only on the legacy adapter, not arbitrary producers.

## Contract and scope

The normative [DVR v1 wire specification](../crates/desktop_visual/protocol.md)
defines exact little-endian layouts, capability masks, size limits, framing and
lifecycle. Frames are top-left, display-referred sRGB RGBA8 with explicit opaque,
straight or premultiplied alpha. No implicit chroma key or fluid-specific meaning
is applied. A frame covers the full physical-pixel X11 root canvas, including all
monitors, and is stretched to that canvas. Per-output routing, HDR, dma-buf,
geometry, input and capture are **not** v1 capabilities. Output extent changes
invalidate the old image and disconnect for a fresh hello; producers reconnect
and rerender. A topology change that leaves the root extent unchanged does not
require new image coordinates under this canvas contract.

One same-UID producer is served at a time. Fixed headers are checked before any
allocation (4096 maximum image axis, 32 MiB maximum padded payload). Strict sequence
ordering is per connection; reconnect begins a fresh sequence epoch. Partial,
malformed, over-limit or timed-out input is discarded and disconnected. The
receiver has a 2-second absolute frame deadline and 30-second idle lease, including
protection against slow trickle input. Single-slot latest-wins replacement plus
socket backpressure prevents queue growth. At most one receiving, one pending and
one renderer-owned CPU payload can coexist. The render loop uses only try-lock and
atomics; it never performs producer IO or waits on a producer-held file lock.
GPU texture upload itself is bounded by the frame limit but is not promised to
be zero-copy or stall-free. This initial path favors a small auditable boundary.

On output change, disconnect or producer failure, no stale overlay is retained.
The GL texture/program belong to JWM. The socket belongs to the receiver, and
cleanup unlinks it only if device/inode still match; unrelated replacement paths
are not removed. Producer processes own their own simulation resources. Same-UID
local access is a trust boundary, not sandboxing of the producer's process.

## Verification

```sh
cargo test --locked -p desktop_visual
cargo clippy --locked -p desktop_visual --all-targets -- -D warnings
python3 -m unittest discover -s extensions/desktop_visual -p 'test_*.py' -v
python3 scripts/test-desktop-visual-gl.py
python3 scripts/test-waterlily-helper.py
python3 scripts/test-showcase-lifecycle.py
cargo check --locked --all-targets --no-default-features --features backend-x11rb
```

Transport tests exercise malformed/truncated frames, limits, deadlines, reconnect,
resize, start/stop, path ownership and latest-frame replacement without a desktop.
They still require permission to create **local Unix sockets**. Do not interpret
sandbox `EPERM`, tests skipped for missing socket permission, or unrun hardware
smokes as a pass. Provider-specific Julia and effect regressions live in juliafun.
