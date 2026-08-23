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
| `jelly` | Five lane-distributed 3D jellyfish adapted from upstream's `ThreeD_Jelly`: pulsing analytic bell membranes roam smoothly along independently seeded x, depth, and height paths; the rose gonad crown inside each translucent bell, four thick curling oral arms, five thin trailing filaments, and the simulated wakes are published with them as a native RGBA volume inside a near-full-screen perspective glass aquarium | violet purple/green |
| `orbit` | Cylinder stirring quiescent fluid along a circular orbit, curling spiral vortex arms | cosmos rose/slate |
| `puddle` | Rain falling into a puddle over the desktop: a damped wave equation whose ripple slopes refract the live screen through the compositor's water-lens contract, with foam on fast crests and pointer-drag wakes | ocean teal/orange |
| `rain` | Rain on fogged glass: a drop-scale force balance in SI units — Furmidge pinning, Cox-Voinov drag, Landau-Levich trails and Marshall-Palmer impacts — pins, grows, merges and runs droplets down the pane, wiping the frost into clear refracting trails; pointer events wipe the mist by hand | glacier azure/bronze |
| `stylus` | Cylinder chasing the mouse pointer through quiescent fluid on a critically damped spring, so every cursor stroke writes vorticity onto the canvas | fluent rainbow |
| `turbulence` | True 3D free turbulence in a periodic tank: randomly oriented Gaussian vortex blobs stretch into filaments, pointer strokes inject depth-localized 3D dipoles, and sparse ambient forcing keeps the cascade alive; vorticity magnitude controls volume density while signed depth-axis vorticity selects the fluent palette's cold/warm side | fluent rainbow |
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
  volume frame -> upload TEXTURE_3D -> near-full-screen perspective glass aquarium
```

Planar cases publish a display-shaped 2D frame exactly as before. Cases with
a true 3D solve (currently `jelly` and `turbulence`) publish their tank as an
RGBA8 voxel volume instead; the compositor uploads it as an OpenGL 3D texture
and ray-marches it inside a projected glass aquarium that fills about 92% of
the limiting viewport axis. The camera stays in front of the tank and adds
small, slow yaw and elevation waves for parallax rather than orbiting behind
it. Its pose derives from the frame timestamp, so re-rendering an unchanged
frame is bit-stable for damage tracking while each new simulation frame
advances the parallax smoothly.

The jelly volume includes the animated anatomy — bell membranes shaded from rim
lavender to apex violet, the rose gonad crown visible through each
translucent bell, four thick curling oral arms, and five thin trailing
filaments — together with the vorticity wakes from the 3D solve.

The turbulence volume contains no synthetic surface or extruded 2D sheet. Its
periodic three-dimensional solve starts from randomly positioned and oriented
divergence-free Gaussian vortex blobs; pointer strokes add counter-signed 3D
dipoles at a smoothly wandering virtual depth, and occasional ambient dipoles
replace energy lost through the forward cascade. Filtered vorticity magnitude
sets a sparse opacity field, while filtered signed depth-axis vorticity chooses
the cold or warm half of the selected diverging palette. A fixed activity floor
leaves quiet water at zero alpha for occupancy skipping, and the rational
density knee is deterministic rather than rescaling and flickering from frame
to frame. `JWM_WATERLILY_PLANAR` retains the signed strongest-magnitude sample
along each depth ray and publishes that projection through the version-1 path.

Both native-volume cases apply a centre-heavy isotropic seven-point filter (the
cell plus its six axial neighbours) to displayed solver vorticity. Jelly
composites its analytic anatomy after filtering, so the filter attenuates
isolated grid impulses without softening bells, arms, organs, or filaments.

The shader performs front-to-back emission/absorption compositing on a fixed,
unjittered midpoint lattice at two samples per voxel. Each step first samples
an R8 occupancy texture built as a one-voxel Chebyshev dilation of every
non-zero source-alpha voxel. This is conservative for the complete 4x4x4
control lattice of the C2-continuous tricubic B-spline, so empty water remains
a one-fetch path without discarding the wider reconstructed tails of thin
membranes, filaments, or vortex rings. Occupied steps reconstruct RGBA with
eight hardware trilinear taps. The lattice keeps a fixed voxel-space phase,
and its last fractional segment is integrated at its true length, so adjacent
rays do not gain or lose a complete opacity slab as their box chord crosses a
step boundary. The first tissue-interface normal comes from a
smoothly wake-gated, one-voxel-scale density field and is evaluated only while
capturing the shallow refracting surface; a bell swimming through its own rough
wake therefore keeps stable desktop refraction without paying that cost along
the rest of the ray.

Low-alpha turbulent wake — both Jelly's simulated wake and every non-empty
turbulence voxel — shades as a normalized Henyey-Greenstein forward-scattering
medium in its own palette hue. Turbulence caps authored alpha at `0x1c`
(`28/255`, below the shader's `0.115` wake ceiling), so even its strongest
vortex remains in this branch and cannot be mistaken for a refracting membrane.
Only Jelly's higher-alpha analytic anatomy enters the smooth participating-
tissue illumination driven by authored apex-to-rim color, world height, and
view-path depth. Deliberately not reapplying per-step Lambert, self-shadow, and
narrow specular to the one-cell display shell prevents its voxel coverage from
reappearing as concentric rings or salt-and-pepper highlights. Per-voxel opacity
remains a strictly monotone transfer instead of flattening ranges into plateaus.
The first Jelly tissue interface is accumulated over a shallow weighted band
for stable scene refraction. A small confidence-gated directional cue is then
applied once to that coherent front interface, rather than independently to
every density layer. Refraction strength uses the same weight and
normal-coherence confidence, so weak or opposing gradients fade continuously
instead of snapping on. A display-referred exponential shoulder rolls off only
accumulated volume highlights before premultiplication, preserving hue without
remapping the transmitted desktop.

The aquarium has perspective-correct front and rear glass rims and a
world-space open water surface below a narrow air gap. Rays through the water
refract the frosted desktop and gain path-length-dependent Beer-Lambert cyan
attenuation; the surface catches grazing reflections, carries a gentle
traveling swell whose crests throw moving glints, and forms a visible
waterline. Like the camera pose, the swell phase derives from the frame
timestamp, so re-rendering an unchanged frame stays bit-stable for damage
tracking. Rays that miss the projected tank stay transparent, leaving the
desktop around its near-full-screen silhouette sharp.

The transmitted desktop is sampled from its mip pyramid with a nine-tap
LOD-2.5 frost kernel. It preserves the broad low-frequency glass lobe of the
former 81-tap pass while avoiding both its cost and full-resolution wallpaper
grain leaking into the volume as speckle. All sampling and camera/swell phases
are deterministic: an unchanged volume and timestamp render bit-identically,
and the marcher deliberately uses no stochastic per-pixel depth jitter.

Focused regressions protect these properties. Julia tests pin each native
producer's non-square `(width, vertical, depth)` geometry and front-to-back
slice mapping. Turbulence-specific coverage verifies a genuine three-component
solve, complete overwrite of the reusable RGBA volume, sparse zero-alpha water,
the `0x1c` material ceiling, cold and warm signed-vorticity colors, palette-
independent alpha, depth variation, and the signed strongest-magnitude planar
fallback. Compositor occupancy unit tests cover one-voxel dilation, edge
clamping, and reuse without stale support; headless real-shader tests compare a
sparse B-spline tail against a no-skip oracle, verify repeatability,
premultiplied output, and preservation of a low-alpha wake's authored hue,
exercise a chord immediately around a fractional-step boundary, and render a
curved analytic bell over a smooth scene texture to reject isolated dark holes,
bright fireflies, and oscillating concentric luma bands. A paired test-only
control with interface confidence disabled confirms that the scene-enabled
regression receives a measurable contribution from the lighting/refraction
path instead of merely drawing tissue over a backdrop.

Volume upload also treats RGBA and occupancy as one coupled snapshot. It
isolates and restores texture-unit, pixel-unpack-buffer, and unpack-alignment
state around transfers; if either upload reports a GL error, both textures are
discarded and rebuilt together on the next publication. A 64 MiB volumetric
frame ceiling bounds the compositor-thread dilation buffers and GPU allocation;
the default accelerated jelly and turbulence volumes each remain below 2 MiB.

This implementation is currently limited to the shared X11 compositor used by
the `x11rb` and `xcb` backends. It is not available on the Wayland backends.
Planar worker frames are stretched to fill the entire output: quiescent
near-white fluid keys out to a frosted blur of the client scene, while motion
lives inside the simulation itself — the default `wander` case roams its body
around the canvas and the wake ripples propagate everywhere it goes. Fluid has
no reference geometry, so the stretch from the simulation aspect ratio to the
display's is not visually objectionable. Volumetric frames instead use the
full-output pass to project the fitted 3D aquarium described above. There is no
per-window target.

WaterLily is rendered in its own compositor pass after client post-processing,
so it does not alter client texture sampling, blur, color/accessibility
processing, or HDR processing. Bright low-chroma pixels from the worker's
opaque background are replaced with a semi-transparent blurred snapshot of the
client scene; colored flow details remain opaque. The blur uses a private
WaterLily scene texture and does not reuse or invalidate client blur caches;
the snapshot is mipmapped after each capture so the broad transmission taps
sample a prefiltered level instead of aliasing photographic wallpaper grain
or text into speckle.
The X11 Composite Overlay Window keeps an empty input shape, making the layer
click-through: pointer and keyboard control continue to target normal client
windows. JWM-owned HUD, transition, and system UI layers remain above
WaterLily. Direct scanout and fullscreen unredirect are suppressed while the
layer is visible because both paths would bypass compositor-owned visuals.

### Rain: a drop-scale force balance

`rain` is the one case whose physics is not a WaterLily solve. Drops on a
vertical pane are a free-surface, moving-contact-line problem, and an
incompressible Navier-Stokes solver over an immersed body cannot represent
either. It is instead a Lagrangian population of sessile drops whose every
rule is a force or a flux written in SI units and converted to canvas pixels
exactly once, through the pane's resolution in pixels per metre:

- **Geometry.** A drop is a spherical cap, `V = π/3·f(θ)·a³` for contact
  radius `a`, so merges, pearls and film losses conserve volume, not radius.
- **Pinning.** A drop hangs while gravity `ρVg` stays below the Furmidge
  retention force `σ·2a·(cos θ_r − cos θ_a)`. That balance sets the critical
  contact radius — about 2.2 mm for water on glass, some 7 px on an 800-tall
  frame — rather than a tuned threshold.
- **Motion.** A runner integrates `m·dU/dt = ρVg − F_retention − k_v·U` with
  Cox-Voinov contact-line dissipation `k_v ∝ μ·a·ln(a/λ)/θ`. Terminal
  velocities land in the 5-20 cm/s measured for 10-50 µL drops on vertical
  glass, and a drop that drains back under the retention force re-pins by
  itself: the hysteresis loop is the physics, not a state flag.
- **Trails.** The moving contact line deposits a Landau-Levich-Derjaguin film
  of thickness `1.34·a·Ca^{2/3}`, gated by the partial-wetting threshold
  `Ca_c ≈ θ_r³/9L`. The deposited rivulet is Rayleigh-Plateau unstable and
  beads up into the residual pearls a runner leaves behind, so the trail, the
  drain rate and the pearls are one conserved volume budget.
- **Impacts.** Sizes are sampled from the Marshall-Palmer distribution
  `N(D) ∝ exp(−Λ D)`, `Λ = 4.1 R^−0.21 mm⁻¹`, at the wind-driven flux onto a
  vertical pane; drops below the tracked cutoff are the mist itself. The
  Cossali-Mundo splash parameter `K = We^{1/2}·Re^{1/4}` decides whether an
  impact throws satellites, and the couple-per-thousand impacts that land
  already above the release radius fall out of the distribution instead of
  being dialled in. A gust raises the rain rate, which flattens the
  distribution and lifts the flux together: squalls arrive as more *and*
  bigger drops.
- **Growth.** Condensation is diffusion limited, `da/dt = K/a`, so small
  drops grow quickly and large ones stall — coalescence, not condensation, is
  what carries most drops over the release radius, as in a real breath
  figure. Coalescence conserves volume and mixes momentum, and tests the
  segment a drop swept during the substep so a fast runner absorbs what it
  passes instead of tunnelling through it.
- **Steering.** A static defect field modulates the local hysteresis, and a
  wetted path relieves it. The cross-stream asymmetry of the retention force
  over the contact line is the lateral force, so meanders and the way runners
  fall into existing channels come out of the same term rather than a sine
  wave.

Two of those interact with the compositor contract rather than with the
water. The frame integrates a runner over an exposure, so drops are drawn
motion-blurred and their optical depth thins as they stretch. And the mist
re-forms by heterogeneous nucleation at a rate that varies across the pane,
with a spinodal-dewetting rush through the half-cleared band: the shader keys
mist to frost only at alpha >= 0.97 and refracts water at low alpha, so a
pixel parked between those states is painted as opaque producer color, and an
aging trail that lingered there read as a milky slab.

There is no hand tracking in this design. It does not use a camera, MediaPipe,
landmarks, or a selected window. The chosen WaterLily case advances on its own
simulation clock; the interactive `puddle`, `rain`, `stylus`, `turbulence`, and
`waltz` cases additionally receive the pointer position, which the compositor
streams to the worker as throttled `pointer X Y` control commands while the
layer is visible. Turbulence maps that 2D point to the tank's x/vertical face
and supplies a slowly varying virtual depth for volumetric stirring.

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
Planar frames are stretched across the display, while volumetric frames are
sampled inside the projected aquarium, so either way the `--sim-size` choice
trades solver cost against on-screen sharpness; `640x400` reads well on common
16:9/16:10 outputs, and `1280x800` is comfortable on a discrete GPU. At
`1280x800`, both Jelly and turbulence cap their `(width, depth, height)` CPU
solve at `96x32x64`, while CUDA and ROCm use the finer `128x48x80` domain.
The higher accelerator ceiling improves curved anatomy and vortex-filament
coverage before tricubic reconstruction; the CPU cap keeps 3D solve and
publication latency practical. Start the worker with `--threads=auto` to keep
the colorize loop parallel.
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
The generic planar transport permits slots up to 512 MiB. A frame with
`depth > 1` has a stricter 64 MiB padded-slot ceiling, checked from the header
before allocating either the tight RGBA result or a stride-compaction buffer;
this bounds the additional occupancy and 3D-texture working set.

In version 1 alpha carries per-case semantics over an opaque contract (the
rain case encodes optical height in inverted alpha; plain cases write `255`).
A version-2 slot is a stack of `depth` planar slices ordered front (nearest
the resting camera) to back, each laid out exactly like a version-1 frame;
voxel RGB is emission color and voxel alpha is the opacity a ray accumulates
crossing one voxel straight through, which the compositor renormalizes by its
actual ray-march step length.

The bundled volume shader also assigns material meaning to producer alpha;
this is a rendering convention layered on the transport, not a new protocol
field. Alpha at or below `0.115` is low-density wake and receives
hue-preserving forward scattering without contributing a tissue-interface
normal. The smooth material transition begins at `0.12` and reaches tissue by
`0.28`; Jelly's analytic anatomy occupies that higher band for coherent
lighting and scene refraction. Turbulence deliberately publishes at most
`0x1c` (`28/255`) and therefore remains entirely below the wake ceiling, while
zero-alpha voxels remain eligible for conservative empty-space skipping.

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
