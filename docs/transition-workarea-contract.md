# Transition workarea contract

Policy passes the intersection of the old tag's workarea, the new tag's
workarea and the selected output to `compositor_notify_tag_switch`, with
`exclude_top=0`. The rectangle already excludes every bar edge. Legacy direct
callers can still pass a top exclusion; this change does not redesign that
interface because the current policy call is consistent.

Coverage added in this change:

- X11 snapshot blit geometry for all four bar edges and hidden bars at 1080p,
  4K and translated secondary-output coordinates; existing tests cover a
  clipped negative origin without stretching the captured part.
- The actual Wayland GLES transition dispatcher at eight exact frame times,
  for all eleven animated modes, four bar edges, hidden bars, a secondary
  output and a partially off-canvas negative origin. Every pixel outside the
  workspace must stay unchanged, the last frame must contain only the new
  scene, and viewport/scissor state must be restored. A positive control
  requires the renderer to change pixels inside the workspace.
- Existing policy tests cover old/new bar visibility, disjoint workareas and
  monitor-local negative coordinates. Existing shader tests cover encoded
  snapshot decoding when rendering into a scene-linear target.

Run with both backend families compiled:

```sh
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 JWM_REQUIRE_HEADLESS_GL=1 \
  scripts/test.sh --lib --no-default-features \
  --features backend-x11rb,backend-xcb,backend-wayland-udev,backend-wayland-nested \
  -- transition_ tag_transition
```

`LIBGL_ALWAYS_SOFTWARE=1` deliberately labels this as Mesa software validation.
Repeat without it on each supported hardware driver. Geometry and shader
tests do not establish real X11 compositor presentation, monitor hotplug,
mixed-DPI desktop behavior or DRM/KMS correctness. Those need a dedicated
desktop/hardware run; no such result is claimed here.
