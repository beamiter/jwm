# Realtime hand slime / refractive-glass effect

The X11 compositor can receive 21 normalized hand landmarks from an external
tracker and reconstruct a smooth hand mask directly on the GPU. Both the
`x11rb` and `xcb` backends use the same `backend/x11/compositor` implementation,
so the effect is implemented only once.

## Why landmarks instead of a dense mask

The tracker sends less than one kilobyte per frame. The compositor turns the
landmarks into circles and capsules in the post-process shader, producing a
continuous metaball-like hand surface. This avoids JSON-encoding a bitmap,
uploading a new mask texture every frame, and coupling model resolution to the
output resolution.

The normal JWM request/response socket remains the control plane. High-rate
poses use a dedicated Unix datagram socket with newest-frame-wins semantics:

```text
$XDG_RUNTIME_DIR/jwm-slime.sock
```

Override it with `JWM_SLIME_SOCKET=/path/to/socket` for testing.

## Demo

Install the optional tracker dependencies:

```bash
python -m pip install opencv-python mediapipe mss
```

Start JWM with either the `x11rb` or `xcb` backend. Find the video player's X11
window ID:

```bash
xwininfo
```

Click the video window, then pass the printed ID to the tracker:

```bash
python tools/slime_tracker.py --window 0x04600007 --fps 30 --debug
```

The compositor automatically enables the effect while valid packets arrive and
fades it out when tracking stops. `--refract-px` controls the displacement.

For a letterboxed video, provide `content_rect` in packets as normalized
`[x, y, width, height]` coordinates inside the window. The demo script currently
captures the whole window.

## Packet format

Each Unix datagram is one compact JSON object:

```json
{
  "version": 1,
  "active": true,
  "window": 73400327,
  "content_rect": [0.0, 0.0625, 1.0, 0.875],
  "refract_px": 12.0,
  "hands": [
    {
      "score": 0.97,
      "landmarks": [[0.51, 0.82], [0.46, 0.71]]
    }
  ]
}
```

`landmarks` must contain the standard 21 MediaPipe hand points. `window` is an
X11 window ID. Omit `window` only when landmarks are already normalized to the
entire screen. Packets naming an unknown window are ignored rather than mapped
to the screen accidentally.

## Render path

1. The tracker captures/inferences outside the compositor.
2. JWM drains all pending datagrams and keeps only the newest valid pose.
3. Window-relative landmarks are mapped to screen pixels.
4. The existing post-process FBO supplies the scene texture.
5. A fragment-shader SDF joins landmark bulbs and hand-bone capsules.
6. The SDF gradient refracts RGB at slightly different offsets and adds a
   Fresnel-like white rim.
7. Existing color correction and HDR tone mapping run afterwards.

While the effect is visible, direct scanout and fullscreen unredirect are
suppressed because they bypass compositor post-processing.

## Production upgrade path

Landmark SDF is the low-latency MVP. For exact wrist and occlusion boundaries,
add an optional 64x64 or 128x128 R8 segmentation mask transported through shared
memory (or dma-buf where available), while retaining landmarks for temporal
stability and analytic highlights.
