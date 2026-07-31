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
| `jelly` | A smack of 3D jellyfish adapted from upstream's `ThreeD_Jelly`: pulsing analytic bell membranes, curved trailing filaments, and their simulated wakes are published together as a native RGBA volume that the compositor ray-marches through a slowly orbiting perspective camera | violet purple/green |
| `orbit` | Cylinder stirring quiescent fluid along a circular orbit, curling spiral vortex arms | cosmos rose/slate |
| `puddle` | Rain falling into a puddle over the desktop: a damped wave equation whose ripple slopes refract the live screen through the compositor's water-lens contract, with foam on fast crests and pointer-drag wakes | ocean teal/orange |
| `rain` | Rain on fogged glass: droplets pin, grow, merge and run down, wiping the frost into clear refracting trails; pointer events wipe the mist by hand | glacier azure/bronze |
| `stylus` | Cylinder chasing the mouse pointer through quiescent fluid on a critically damped spring, so every cursor stroke writes vorticity onto the canvas | fluent rainbow |
| `turbulence` | Free two-dimensional turbulence: random seeded vortices merge and strain into filaments while pointer strokes stir new dipoles in and an ambient reseed keeps the canvas alive | fluent rainbow |
| `waltz` | Cylinder following the mouse pointer through a uniform stream with dance's transverse heave riding the chase spring, trailing the braided wake from the cursor | mica teal/amethyst |
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
              | (version 1: planar frame; version 2: RGBA voxel volume)
              v
JWM X11 compositor
  planar frame -> upload TEXTURE_2D -> full-screen frosted canvas layer
  volume frame -> upload TEXTURE_3D -> perspective ray-marched 3D tank
```

Planar cases publish a display-shaped 2D frame exactly as before. Cases with
a true 3D solve (currently `jelly`) publish their tank as an RGBA8 voxel
volume instead; the compositor uploads it as a 3D texture and ray-marches it
natively with an orbiting perspective camera. The volume includes both the
animated bell membranes, curved volumetric filaments, and the vorticity wakes
from the 3D solve. The shader
performs front-to-back emission/absorption compositing, reconstructs material
normals from three-dimensional density gradients, traces short light rays for
self-shadowing, and applies directional lighting, Fresnel highlights, depth
haze, and scene refraction at the first coherent surface. Stable midpoint
integration samples in voxel units, while a sub-voxel five-tap footprint in
the camera plane prefilters thin vortex sheets before they are magnified to
the desktop. Together they avoid both temporal glitter and the point-cloud
salt-and-pepper pattern produced by infinitesimal rays through a coarse 3D
field. The transfer function keeps turbulent wake less absorbing than bell
tissue, and the shader floors multiple-scattered ambient light so stacked
translucent layers cannot collapse to black. The camera pose
derives from the frame timestamp, so re-rendering an unchanged frame is
bit-stable for damage tracking while each new simulation frame advances the
orbit. Empty tank water reveals the same frosted desktop backdrop the planar
shader keys out, so both paths keep one glass look.

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
landmarks, or a selected window. The chosen WaterLily case advances on its own
simulation clock; the interactive `stylus` and `waltz` cases additionally
receive the pointer position, which the compositor streams to the worker as
throttled `pointer X Y` control commands while the layer is visible.

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
- `cuda`: NVIDIA execution using `CUDA.CuArray` (default). Loaded directly;
  if CUDA is missing or not functional the worker exits with the load error.
- `rocm`: AMD execution using `AMDGPU.ROCArray`.
- `auto`: probe for an available accelerator and otherwise use the CPU.

CUDA or ROCm requires a working vendor driver and its Julia package to already
be available in the project. The runner does not install packages at runtime.
Regardless of simulation device, the worker publishes the final visualization
as tightly packed RGBA8. This preserves the example's color map rather than
asking JWM to reconstruct color from pressure or velocity fields.

When CUDA.jl is configured to use a local toolkit and
`waterlily/.cuda-toolkit` exists, the worker automatically exposes that path
as `CUDA_PATH`. Set `JWM_WATERLILY_CUDA_PATH` to use a local toolkit at a
different path. This avoids downloading Julia's multi-gigabyte CUDA runtime
artifact when a compatible toolkit is already installed elsewhere.

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
| `JWM_WATERLILY_PLANAR` | Worker-side: force volumetric cases through their planar 2D projection (version-1 frames), for consumers without volumetric support |

The socket and frame-file values supplied to JWM and the worker must match.
The published simulation frame is stretched to cover the display, so the
`--sim-size` choice trades solver cost against on-screen sharpness; `640x400`
reads well on common 16:9/16:10 outputs, and `1280x800` is comfortable on a
discrete GPU. Start the worker with `--threads=auto` to keep the colorize loop parallel.
Publishing is paced against an absolute schedule and the solver advances
under a per-frame time budget: when the simulation cannot reach real time
within the budget, the publish cadence stays fixed and the simulation clock
dilates into smooth slow motion instead of stuttering. The worker logs the
sustained simulation speed when it stays below real time; reduce
`--sim-size` for real-time playback.

## Frame-file protocol

The frame file begins with a fixed little-endian header. Two equally sized
pixel slots follow it. Version 1 describes a planar frame with a 64-byte
header; version 2 describes a voxel volume with a 96-byte header whose first
64 bytes are the version-1 prefix byte-for-byte.

| Offset | Size | Type | Field and required value |
| ---: | ---: | --- | --- |
| 0 | 8 | bytes | magic `JWMLILY\0` |
| 8 | 4 | `u32` LE | version, `1` planar or `2` volumetric |
| 12 | 4 | `u32` LE | header length, `64` (v1) or `96` (v2) |
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
| 64 | 4 | `u32` LE | (v2 only) depth in slices, at least `1` |
| 68 | 28 | bytes | (v2 only) reserved, zero |

For a header length `H`, a slot size `S = stride * height * depth` (depth is
`1` in version 1), the byte ranges are:

```text
header: [0, H)
slot 0: [H, H + S)
slot 1: [H + S, H + 2*S)
total file length: H + 2*S
```

Both versions use top-to-bottom rows and R, G, B, A byte order. `stride` must
be at least `width * 4`; the built-in worker writes tight rows with equality.
Width, height, depth, stride, slot, checked slot offsets, and total file
length must all validate before JWM uploads a frame.

In version 1 alpha carries per-case semantics over an opaque contract (the
rain case encodes optical height in inverted alpha; plain cases write `255`).
A version-2 slot is a stack of `depth` planar slices ordered front (nearest
the resting camera) to back, each laid out exactly like a version-1 frame;
voxel RGB is emission color and voxel alpha is the opacity a ray accumulates
crossing one voxel straight through, which the compositor renormalizes by its
actual ray-march step length.

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
3. either reducing a 3D field to a 2D view, or advertising a volumetric
   `frame_geometry` and colorizing the 3D field with `render_volume!` so the
   compositor ray-marches it natively (keep a planar path too — it serves the
   `JWM_WATERLILY_PLANAR` fallback);
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
