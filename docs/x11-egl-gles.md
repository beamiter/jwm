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
compositor_api = "egl" # "glx" or "auto"
```

`JWM_COMPOSITOR_API=egl|glx|auto` overrides the file for one launch. This is
useful for recovery if a driver update breaks one path. Graphics API selection happens
when the compositor starts, so changing `compositor_api` requires restarting JWM.

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

## Diagnostics

Startup logs include the selected API (`glx/opengl` or `egl/gles3`). To compare
paths without changing config:

```sh
JWM_COMPOSITOR_API=egl JWM_COMPOSITOR=1 jwm --backend x11rb
JWM_COMPOSITOR_API=egl JWM_COMPOSITOR=1 jwm --backend xcb
JWM_COMPOSITOR_API=glx JWM_COMPOSITOR=1 jwm --backend x11rb
```

When `auto` falls back, the EGL initialization error is logged before GLX is
created. A forced `egl` selection fails compositor initialization instead of
silently changing APIs, and JWM continues in its existing non-composited
fallback mode.
