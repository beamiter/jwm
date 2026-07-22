# WaterLily compositor layer

JWM can composite colored frames produced by an external Julia
[WaterLily.jl](https://github.com/WaterLily-jl/WaterLily.jl) worker as a
full-screen canvas layer. The Julia process owns the simulation and can run it
on the CPU, an NVIDIA GPU, or an AMD GPU. JWM owns only the frame transport,
texture upload, opacity, and final compositor rendering.

The built-in adapters are independently authored simulations using WaterLily's
public `AutoBody` and `Simulation` APIs:

| Case | Effect | Palette |
| --- | --- | --- |
| `hover` | Heaving, pitching thin plate, visually aligned with upstream's [`TwoD_Hover`](https://github.com/WaterLily-jl/WaterLily-Examples/blob/58792dd17cfe585f7f4eea8be925de1b4ffefa25/examples/TwoD_Hover.jl) example without copying its implementation | seismic blue/red |
| `cylinder` | Static circular cylinder shedding the classic von Kármán vortex street | ocean teal/orange |
| `dance` | Cylinder oscillating transversely to the stream, weaving a wide braided wake | violet purple/green |
| `flap` | Plate pitching about its leading edge, producing a thrust-type reverse Kármán wake | ember indigo/amber |
| `tandem` | Two static cylinders in tandem with interfering, merging vortex streets | glacier azure/bronze |
| `diamond` | Square prism rotated 45° whose sharp edges shed a wide, angular street | berry magenta/lime |
| `orbit` | Cylinder stirring quiescent fluid along a circular orbit, curling spiral vortex arms | cosmos rose/slate |
| `wander` | Cylinder roaming quiescent fluid on a smooth non-repeating Lissajous path, trailing its wake across the whole canvas (default) | aurora teal/magenta |

Every diverging palette shares the same near-white midpoint, so the compositor
shader's bright/low-chroma keying replaces quiescent fluid with the frosted
backdrop regardless of the selected case. The long-term goal is a case
registry that can adapt the 2D and 3D simulations in
[WaterLily-Examples](https://github.com/WaterLily-jl/WaterLily-Examples)
without coupling the Rust compositor to Julia packages or case-specific fields.

## Architecture and scope

```text
Julia WaterLily worker
  CPU Array / CUDA CuArray / AMDGPU ROCArray
              |
              | RGBA8 double-buffer file + Unix socket wakeups
              v
JWM X11 compositor
  upload texture -> full-screen, frosted, input-transparent canvas layer
```

This implementation is currently limited to the shared X11 compositor used by
the `x11rb` and `xcb` backends. It is not available on the Wayland backends.
The worker frame is stretched to fill the entire output as a full-screen
canvas: quiescent near-white fluid keys out to a frosted blur of the client
scene across the whole display, while motion lives inside the simulation
itself — the default `wander` case roams its body around the canvas and the
wake ripples propagate everywhere it goes. Fluid has no reference geometry,
so the stretch from the simulation aspect ratio to the display's is not
visually objectionable. There is no per-window target.

WaterLily is rendered in its own compositor pass after client post-processing,
so it does not alter client texture sampling, blur, color/accessibility
processing, or HDR processing. Bright low-chroma pixels from the worker's
opaque background are replaced with a semi-transparent blurred snapshot of the
client scene; colored flow details remain opaque. The blur uses a private
WaterLily scene texture and does not reuse or invalidate client blur caches.
The X11 Composite Overlay Window keeps an empty input shape, making the layer
click-through: pointer and keyboard control continue to target normal client
windows. JWM-owned HUD, transition, and system UI layers remain above
WaterLily. Direct scanout and fullscreen unredirect are suppressed while the
layer is visible because both paths would bypass compositor-owned visuals.

There is no hand tracking in this design. It does not use a camera, MediaPipe,
landmarks, a selected window, or pointer motion. The chosen WaterLily case
advances on its own simulation clock.

## Quick start

Instantiate the checked-in Julia environment once:

```bash
julia --project=waterlily -e 'using Pkg; Pkg.instantiate()'
```

Build and start JWM with either supported X11 backend:

```bash
cargo build

JWM_BACKEND=x11rb \
JWM_WATERLILY_ENABLED=1 \
target/debug/jwm
```

In a second terminal, start the worker:

```bash
julia --project=waterlily --threads=auto waterlily/runner.jl \
  --case wander \
  --device auto \
  --fps 30
```

Swap `--case wander` for any other registered case to select the starting
effect; `--help` prints the current registry. A running worker can also be
switched live — see "Hot-switching cases" below.

Use `JWM_BACKEND=xcb` to exercise the other X11 frontend. The compositor code
and the frame protocol are shared by both.

The default endpoint and frame file are:

```text
$XDG_RUNTIME_DIR/jwm-waterlily.sock
$XDG_RUNTIME_DIR/jwm-waterlily.frame
```

When `XDG_RUNTIME_DIR` is unavailable, JWM uses the private
`/tmp/jwm-$UID/` runtime directory. Explicit paths are preferable for Xephyr
tests:

```bash
JWM_BACKEND=x11rb \
JWM_WATERLILY_ENABLED=1 \
JWM_WATERLILY_SOCKET=/tmp/jwm-waterlily-test.sock \
JWM_WATERLILY_FRAME_FILE=/tmp/jwm-waterlily-test.frame \
target/debug/jwm
```

```bash
julia --project=waterlily --threads=auto waterlily/runner.jl \
  --case wander \
  --device cpu \
  --fps 30 \
  --sim-size 640x400 \
  --socket /tmp/jwm-waterlily-test.sock \
  --frame-file /tmp/jwm-waterlily-test.frame
```

The same commands are collected in `scripts/xephyr.sh` as a small manual smoke
test. Start JWM before the worker so the wakeup socket exists.

## CPU, CUDA, and ROCm

The `--device` option accepts:

- `cpu`: ordinary Julia `Array` storage.
- `cuda`: NVIDIA execution using `CUDA.CuArray`.
- `rocm`: AMD execution using `AMDGPU.ROCArray`.
- `auto`: select an available accelerator and otherwise use the CPU.

CUDA or ROCm requires a working vendor driver and its Julia package to already
be available in the project. The runner does not install packages at runtime.
Regardless of simulation device, the worker publishes the final visualization
as tightly packed RGBA8. This preserves the example's color map rather than
asking JWM to reconstruct color from pressure or velocity fields.

To keep the checked-in CPU environment small, install a GPU backend in a named
local Julia environment and develop the worker into it:

```bash
# NVIDIA
julia --project=@jwm-waterlily-gpu -e \
  'using Pkg; Pkg.develop(path="waterlily"); Pkg.add("CUDA")'

# AMD (use this instead of CUDA)
julia --project=@jwm-waterlily-gpu -e \
  'using Pkg; Pkg.develop(path="waterlily"); Pkg.add("AMDGPU")'
```

Then replace `--project=waterlily` with
`--project=@jwm-waterlily-gpu` when starting the worker.

## Runtime controls

The default `Alt+Shift+F11` binding invokes the canonical action
`toggle_waterlily`. It can also be sent over IPC:

```bash
jwm-tool msg toggle_waterlily
```

### Hot-switching cases

The wake socket is bidirectional: the compositor writes newline-terminated
control commands (`case <name>` or `case next`) back to the connected worker,
which rebuilds the requested simulation at the current resolution without
restarting or touching the frame file. The default `Alt+Shift+F10` binding
invokes the `waterlily_case` action with the `next` argument, cycling the
worker's sorted registry. Over IPC:

```bash
# Cycle to the next registered case
jwm-tool msg waterlily_case

# Select a specific case
jwm-tool msg waterlily_case --args '"dance"'
```

Case names are restricted to short lowercase identifiers on the compositor
side, and the worker validates them against its registry, so a compositor
with a stale case list logs a warning instead of wedging the worker. If no
worker is connected the request is dropped with a log message.

The following environment variables are read when the integration starts:

| Variable | Purpose |
| --- | --- |
| `JWM_WATERLILY_SOCKET` | Unix socket used for worker wakeup/control messages |
| `JWM_WATERLILY_FRAME_FILE` | Shared double-buffer frame file |
| `JWM_WATERLILY_ENABLED` | Initial enabled state (`1`/`true` enables it) |
| `JWM_WATERLILY_OPACITY` | Layer blend opacity, clamped to `0..1` |

The socket and frame-file values supplied to JWM and the worker must match.
The published simulation frame is stretched to cover the display, so the
`--sim-size` choice trades solver cost against on-screen sharpness; `640x400`
reads well on common 16:9/16:10 outputs, and `1280x800` is comfortable on a
discrete GPU. The worker pipelines each frame — the device solver advances
the next state while host threads colorize and publish the previous one — so
start it with `--threads=auto` to keep the renderer parallel. Raw solver time
per published frame scales with the case's length scale (its body size), so a
case that overruns the frame budget degrades to a lower publish rate instead
of stuttering.

## Version 1 frame-file protocol

The frame file begins with a fixed 64-byte little-endian header. Two equally
sized pixel slots follow it.

| Offset | Size | Type | Field and required value |
| ---: | ---: | --- | --- |
| 0 | 8 | bytes | magic `JWMLILY\0` |
| 8 | 4 | `u32` LE | version, `1` |
| 12 | 4 | `u32` LE | header length, `64` |
| 16 | 4 | `u32` LE | width in pixels |
| 20 | 4 | `u32` LE | height in pixels |
| 24 | 4 | `u32` LE | row stride in bytes |
| 28 | 4 | `u32` LE | pixel format, `1` = RGBA8 |
| 32 | 4 | `u32` LE | color space, `1` = sRGB |
| 36 | 4 | `u32` LE | alpha mode, `1` = opaque |
| 40 | 4 | `u32` LE | origin, `1` = top-left |
| 44 | 4 | `u32` LE | published slot, `0` or `1` |
| 48 | 8 | `u64` LE | monotonically increasing sequence |
| 56 | 8 | `u64` LE | producer timestamp in nanoseconds |

For a slot size `S = stride * height`, the byte ranges are:

```text
header: [0, 64)
slot 0: [64, 64 + S)
slot 1: [64 + S, 64 + 2*S)
total file length: 64 + 2*S
```

Version 1 uses top-to-bottom rows and R, G, B, A byte order. `stride` must be at
least `width * 4`; the built-in worker writes tight rows with equality. Alpha is
opaque, so producers should write `255` in every alpha byte. Width, height,
stride, slot, checked slot offsets, and total file length must all validate
before JWM uploads a frame.

The producer takes an exclusive advisory file lock, writes a complete
non-published slot, publishes its slot, sequence, and timestamp, and then
releases the lock and sends a wakeup. JWM holds a shared lock while reading the
header and pixels. JWM retains latest-frame semantics, so stale sequences can be
dropped. A missing, truncated, malformed, or unsupported frame file disables
that update without treating its bytes as pixels.

## Case registry and future adapters

`waterlily/runner.jl` is the stable worker entry point. `--case` selects a
registered adapter; unknown names must fail with a useful list rather than
silently choosing a different simulation. An adapter is responsible for:

1. constructing its WaterLily simulation for the selected memory device;
2. advancing the solver independently of display refresh;
3. reducing a 3D field to a 2D view when necessary;
4. applying the case's intended color map and producing RGBA8;
5. publishing only complete frames through the common writer.

New ports should start from the upstream example's geometry, boundary
conditions, numerical parameters, and color mapping. Keep transport and CLI
behavior case-independent so adding examples does not require compositor
changes.

## Migration from the retired interaction effect

The old IPC/config action `toggle_slime` remains accepted temporarily as a
deprecated alias for `toggle_waterlily` and logs a migration warning. It is not
advertised by `get_capabilities`, is not used by the default key binding, and
must not be used in new configuration. The former Python tracker/demo tools and
their `JWM_SLIME_*` tuning variables are no longer supported.
