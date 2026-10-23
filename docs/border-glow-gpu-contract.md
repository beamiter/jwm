# Border Glow GPU contract

The headless GL harness now runs the real X11 GL 3.3 and Wayland GLES 3
border shaders for twelve radius/spread/alpha combinations. Tests compare
the complete frames across APIs (two-byte tolerance), check premultiplied
alpha for every pixel, and compare straight-edge falloff to a CPU oracle
(three-byte tolerance). Zero alpha must draw nothing, and the window
interior must remain transparent to the glow underlay.

A separate test launches its config-mutating child in an isolated process.
It exercises the actual Wayland compositor for settled-frame suppression,
window movement, retained partial damage versus a full redraw, focus changes,
hot disable/recolor, fullscreen suppression, and a changing wallpaper below a
frosted client. It prints the actual GL repair scissor and repaired pixel area.
The child is marked ignored solely to prevent concurrent standalone execution;
the non-ignored parent always runs it with `--exact --ignored` and fails if it
does not pass. It is not an unexecuted coverage gap.

```sh
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 JWM_REQUIRE_HEADLESS_GL=1 \
  scripts/test.sh --lib --no-default-features \
  --features backend-x11rb,backend-xcb,backend-wayland-udev,backend-wayland-nested \
  -- border_glow_ --nocapture
```

Software Mesa results validate shader math and the tested retained-rendering
path. They do not establish discrete-GPU driver behavior, physical display
scanout, a live X11 compositor, or a real Astronomy wallpaper/transparent-bar
session. Existing transparent-bar, scene-linear, wallpaper and scanout tests
remain required. Run the combined effects on real hardware before treating
that acceptance matrix as complete; no rendering/performance bug is assumed
from missing hardware coverage.
