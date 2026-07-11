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
Motion-filtered fingertip landmarks inject short capsule impulses into a
half-resolution, persistent `RG16F` wave field. Two ping-pong textures retain the
current and previous height while a 9-tap isotropic Verlet step propagates each
wake and applies amplitude-dependent damping. The field remains alive for roughly
1.5 seconds independently of the current pose.

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
  "seq": 1204,
  "timestamp_ns": 998877665544,
  "hands": [
    {
      "score": 0.97,
      "landmarks": [[0.51, 0.82], [0.46, 0.71]]
    }
  ]
}
```

`landmarks` must contain the standard 21 MediaPipe hand points. `window` is an
X11 window ID. Omit `window` only when points are normalized to the entire
screen. Packets naming an unknown/stale XID are ignored instead of being mapped
to the screen accidentally. Unknown fields such as `seq` and `timestamp_ns` are
forward-compatible diagnostics.

## Render path

1. The tracker captures and runs inference outside JWM.
2. JWM retains only the newest datagram.
3. Window-relative landmarks are mapped and adaptively smoothed in screen pixels.
   Five fingertip tracks emit distance-spaced ripple events while rejecting
   stationary landmark jitter.
4. The existing post-process FBO supplies the composited scene texture.
   A feathered 9-tap cool blur covers the selected window as the calm water
   surface; packets without a window apply the skin to the whole screen.
5. The GPU runs two wave-equation substeps per display frame. Old disturbances
   keep propagating after the hand disappears and then damp out automatically.
6. A 9-sample Sobel gradient and Laplacian drive refraction, lens curvature,
   chromatic dispersion, Schlick Fresnel, dual-lobe specular, and caustics. A
   low-opacity hand SDF provides secondary tracking feedback.
7. Existing accessibility, color correction, and HDR passes run afterwards.
8. The pose is held briefly and faded when inference disappears or stops.

## Current limitations and production path

The landmark SDF is a low-latency MVP. It approximates the wrist boundary and
cannot express hand/object occlusion precisely. Screen capture also sees any
window covering the selected video region.

For production quality, add an optional 64x64 or 128x128 R8 hand-segmentation
mask transported through shared memory (or dma-buf where practical). Keep the
landmarks for temporal stabilization, analytic highlights, and fallback when
the segmentation model drops a frame.
