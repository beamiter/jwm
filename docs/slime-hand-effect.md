# Realtime hand slime / refractive-glass effect

The shared X11 compositor can receive 21 normalized hand landmarks from an
external tracker and reconstruct a smooth hand-shaped mask directly on the GPU.
Both the `x11rb` and `xcb` backends use `backend/x11/compositor`, so the render
implementation is shared.

## Architecture

The normal JWM request/response socket remains the control plane. High-rate
poses use a dedicated, lossy Unix datagram socket:

```text
$XDG_RUNTIME_DIR/jwm-slime.sock
```

Override the location with `JWM_SLIME_SOCKET=/path/to/socket`.

A receiver thread continuously replaces a single pending packet. The compositor
therefore consumes the newest pose rather than replaying stale inference frames.
Motion-filtered fingertip and palm landmarks inject directional momentum capsules
into a persistent, half-resolution `RGBA16F` fluid field. The channels retain
surface height, horizontal velocity, and foam. A fixed 120 Hz semi-Lagrangian
shallow-water solver adds gravity, divergence coupling, viscosity, vorticity
enhancement, crest foam, gesture swirl, and an absorbing boundary. Persistent
ocean waves are analytic and therefore do not consume simulation bandwidth when
the tracker is idle. The interactive field remains alive for roughly 1.5 seconds
independently of the current pose; after that the GPU field is cleared once and
the numerical solver sleeps until the next gesture.

While the effect is visible, direct scanout and fullscreen unredirect are
suppressed because both paths bypass compositor post-processing.

## 1. Verify the compositor without a model

Start JWM using either the `x11rb` or `xcb` backend, then run the dependency-free
synthetic pose generator:

```bash
python tools/slime_pose_demo.py
```

This draws an animated glass hand in screen coordinates. To constrain it to a
video window, click the window with `xwininfo`, copy its ID, and run:

```bash
python tools/slime_pose_demo.py --window 0x04600007 --refract-px 14
```

This test isolates the compositor shader and IPC path from MediaPipe, capture
permissions, and model latency.

## 2. Run the real hand tracker

Install the optional Python dependencies and X11 utility:

```bash
sudo apt install x11-utils
python -m pip install opencv-python mediapipe mss numpy
```

Run the tracker without `--window` and click the video player when prompted:

```bash
python tools/slime_tracker.py --fps 30 --debug
```

A known X11 ID can be supplied directly:

```bash
python tools/slime_tracker.py \
  --window 0x04600007 \
  --fps 30 \
  --max-width 640 \
  --max-height 640 \
  --refract-px 12
```

The capture is downscaled only for inference; landmarks stay normalized and are
mapped back to the live compositor window geometry.

### Letterboxed or UI-wrapped video

`--content-rect` crops inference to the actual video and tells JWM how to map the
normalized landmarks back into the window. It uses `x,y,width,height` in
normalized window coordinates. For example, to omit 6.25% at the top and bottom:

```bash
python tools/slime_tracker.py \
  --window 0x04600007 \
  --content-rect 0,0.0625,1,0.875
```

The tracker sends one inactive transition when a hand disappears; stale active
poses also time out automatically, so a lost datagram cannot leave the effect
stuck on screen.

## Packet format

Each Unix datagram contains one compact JSON object:

```json
{
  "version": 1,
  "active": true,
  "window": 73400327,
  "content_rect": [0.0, 0.0625, 1.0, 0.875],
  "refract_px": 12.0,
  "ocean_strength": 0.32,
  "turbulence_strength": 0.68,
  "foam_strength": 0.78,
  "seq": 1204,
  "timestamp_ns": 998877665544,
  "hands": [
    {
      "score": 0.97,
      "landmarks": [[0.51, 0.82, -0.04], [0.46, 0.71, -0.06]]
    }
  ]
}
```

`landmarks` must contain the standard 21 MediaPipe hand points. Both legacy
`[x,y]` points and depth-aware `[x,y,z]` points are accepted in version 1. A
negative MediaPipe Z value brings the corresponding liquid capsule toward the
viewer and increases its apparent thickness. The three fluid controls are
optional and are clamped to `[0,1]`.

`window` is an X11 window ID. Omit `window` only when points are normalized to
the entire screen. Packets naming an unknown/stale XID are ignored instead of
being mapped to the screen accidentally. Unknown fields such as `seq` and
`timestamp_ns` remain forward-compatible diagnostics.

## Render path

1. The tracker captures and runs inference outside JWM.
2. JWM retains only the newest datagram.
3. Window-relative XY and optional Z landmarks are adaptively smoothed. Fingertips
   inject narrow wakes while the palm injects a broader momentum wake; stationary
   tracking jitter is rejected by distance thresholds.
4. The existing post-process FBO supplies the composited scene texture. A
   feathered 9-tap cool blur covers the selected window as the calm liquid
   baseline; packets without a window apply the skin to the whole screen.
5. The GPU catches up in fixed 120 Hz fluid substeps, independent of display
   refresh. Height and horizontal velocity are semi-Lagrangian advected, coupled
   through a shallow-water pressure term, diffused, and augmented with vorticity.
6. Multi-direction analytic ocean waves are added to the simulated height. A
   Sobel gradient, transported micro-normal, and foam field drive refraction,
   chromatic dispersion, Schlick Fresnel, specular highlights, and caustics.
7. The depth-aware hand SDF supplies a quiet three-dimensional tracking guide.
8. Existing accessibility, color correction, and HDR passes run afterwards.
9. The pose is held briefly and faded when inference disappears or stops.

## Runtime tuning

The simulation defaults to half resolution. Set `JWM_SLIME_SIM_SCALE` before
starting JWM to trade GPU cost for detail:

```bash
JWM_SLIME_SIM_SCALE=0.35 jwm   # low-power
JWM_SLIME_SIM_SCALE=0.50 jwm   # default
JWM_SLIME_SIM_SCALE=0.75 jwm   # high quality
```

Values are clamped to `0.25..1.0`. Tracker controls can be changed independently:

```bash
python tools/slime_tracker.py \
  --ocean-strength 0.32 \
  --turbulence-strength 0.68 \
  --foam-strength 0.78
```

## Current limitations and production path

The field is a nonlinear 2.5D shallow-water approximation, not a volumetric
three-dimensional Navier-Stokes solver. It produces directional flow, vortices,
foam, depth-shaped capsules, and convincing screen-space lighting, but cannot
overturn into breaking geometry or collide with unknown objects inside client
windows. Screen capture also sees any window covering the selected video region.

For production-quality silhouettes, add an optional 64x64 or 128x128 R8 hand
segmentation mask transported through shared memory (or dma-buf where practical).
Keep landmarks for depth, temporal stabilization, analytic highlights, momentum
injection, and fallback when the segmentation model drops a frame.
