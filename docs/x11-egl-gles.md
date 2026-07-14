# X11 EGL/GLES compositor

The `x11rb` and `xcb` backends share one X11 compositor and can render through
one of two graphics platforms:

- `glx`: desktop OpenGL with `GLX_EXT_texture_from_pixmap` (the compatibility
  default).
- `egl`: OpenGL ES 3 through EGL. XComposite named pixmaps are imported with
  `EGL_KHR_image_pixmap` and bound with `GL_OES_EGL_image`.
- `auto`: try EGL/GLES first and fall back to GLX if the required EGL config or
  pixmap-image extensions are unavailable.

Configure the platform in `config.toml`:

```toml
[behavior]
compositor = true
compositor_api = "egl" # "glx" or "auto"; "gles" is an alias for "egl"
```

`JWM_COMPOSITOR_API=egl|gles|glx|auto` overrides the file for one launch. This
is useful for recovery if a driver update breaks one path. Graphics API
selection happens when the compositor starts, so changing `compositor_api`
requires restarting JWM.

## Requirements

The EGL path requires:

- EGL with an X11 window config matching the compositor overlay visual;
- an OpenGL ES 3 context;
- `EGL_KHR_image_base` (or `EGL_KHR_image`);
- `EGL_KHR_image_pixmap`;
- `GL_OES_EGL_image` / `glEGLImageTargetTexture2DOES`.

The implementation keeps X Damage, X Present, resize handling, animations,
blur, screenshots, and HDR configuration in the shared compositor. GLSL 330
sources are translated to GLSL ES 300 at compile time and use separate shader
cache keys. `GLX_OML_sync_control` remains GLX-only; selecting it with EGL
falls back to global platform vsync.

## Partial redraw

When the driver exposes `EGL_EXT_buffer_age` or `EGL_KHR_partial_update`, JWM
tracks a bounded history of scene changes. A recycled back buffer is repaired
by merging the current X Damage region with the changes from the missing
frames. With partial-update support, that merged buffer-damage region is sent
to EGL before any drawing so the producer can skip untouched tiles. An
undefined buffer (age zero), an unusually old buffer, resize, or failed safety
requirement falls back to a full redraw. This avoids the per-frame copy-back
dependency of preserved swap. On drivers without buffer-age support, JWM
retains the previous `EGL_BUFFER_PRESERVED` compatibility path when the
matching EGLConfig supports it.

If the driver also exposes `EGL_KHR_swap_buffers_with_damage` or
`EGL_EXT_swap_buffers_with_damage`, the current frame's disjoint dirty
rectangles are passed in bottom-left-origin coordinates to the surface swap.
Rendering keeps a single merged repair scissor, while the window system can
avoid processing unchanged pixels between distant updates. A driver that
rejects an advertised buffer-age query or damage-swap entry point is downgraded
once for that surface. Multi-monitor wallpaper passes intersect each monitor
with the repair scissor, and the scissor remains active through post-processing
and overlays instead of reverting to full-screen work late in the frame.

## Window import hot path

Window visual and depth are immutable after X11 window creation. JWM caches the
format metadata needed by the selected API with each imported texture, so resize
bursts can recreate named pixmaps without synchronous `GetWindowAttributes` /
`GetGeometry` requests. GLES only queries depth on first import; GLX sends both
format requests before waiting for their replies. Damage refreshes also avoid
creating per-window GL fences that are not consumed by a later rendering
decision. The initial dirty-region scan also records whether GLES native
texture synchronization is needed, avoiding another scene lookup pass. During
resize bursts, the synchronization performed before pixmap import is reused by
the ensuing texture refresh instead of issuing a second native round trip.

Output enumeration reports refresh rates in millihertz, while compositor policy
uses rounded whole Hz. Startup logs therefore show both the precise rate (for
example `120.081Hz`) and the policy value (`120Hz`).

## Diagnostics

Startup logs include the selected API (`glx/opengl` or `egl/gles3`) and EGL
partial-redraw capability state. To compare paths without changing config:

```sh
JWM_COMPOSITOR_API=egl JWM_COMPOSITOR=1 jwm --backend x11rb
JWM_COMPOSITOR_API=egl JWM_COMPOSITOR=1 jwm --backend xcb
JWM_COMPOSITOR_API=glx JWM_COMPOSITOR=1 jwm --backend x11rb
```

When `auto` falls back, the EGL initialization error is logged before GLX is
created. A forced `egl` selection fails compositor initialization instead of
silently changing APIs, and JWM continues in its existing non-composited
fallback mode.
