# Client window border glow

JWM can render a static, directional outer glow around client windows on both
its native X11 compositor and the direct Wayland DRM/KMS compositor. The effect
uses one rounded-rectangle SDF draw per selected window; it does not allocate a
per-window blur framebuffer and it does not require continuous animation frames.

```toml
[behavior]
border_enabled = true
border_width = 2.0
border_color_focused = [0.35, 0.96, 1.00, 1.00]

border_glow_enabled = true
border_glow_focused_only = true
border_glow_radius = 28.0
border_glow_intensity = 1.0
border_glow_color = [0.00, 0.55, 1.00, 0.38]
border_glow_include = ["JTerm4"]
border_glow_exclude = []
```

`border_glow_include` and `border_glow_exclude` use case-insensitive substring
matching against the X11 class or Wayland app-id. An empty include list selects
all ordinary client windows; exclusions take precedence.

The built-in luminance profile intentionally makes the top/right edge and the
upper-right hotspot brighter, with a weaker lower-left hotspot. Focused-only is
the default so a single active client reads as the light source.

Fullscreen, shaped, and X11 override-redirect surfaces are skipped. In
particular, suppressing the effect for fullscreen clients preserves direct
scanout/unredirect eligibility. Partial-damage regions are expanded by the glow
radius, and backdrop-blur cache keys include the effective glow style.
