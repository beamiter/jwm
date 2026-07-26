# Gradient window border

JWM draws the focused window's border as a two-color linear gradient ring
(enabled by default) on both its native X11 compositor and the direct Wayland
DRM/KMS compositor. Set `border_gradient_enabled = false` to return to the
flat `border_color_focused`.
The ring uses the same rounded-rectangle SDF mask as the flat border, with
outer corners kept concentric to the window's corner radius, so it composes
cleanly with `corner_radius`, shadows, and the border glow.

```toml
[behavior]
border_enabled = true
border_width = 2.0

border_gradient_enabled = true
border_gradient_color_a = [0.24, 0.65, 1.00, 1.00]
border_gradient_color_b = [0.72, 0.35, 1.00, 1.00]
border_gradient_angle = 45.0
border_gradient_speed = 0.0
```

`border_gradient_angle` is the gradient direction in degrees: `0` runs
left→right, `90` top→bottom. The quad's extreme corners always map to exactly
color A and color B regardless of angle.

`border_gradient_speed` rotates the direction in degrees per second. `0`
(the default) keeps the gradient fully static and costs nothing beyond the
flat border draw. A non-zero speed keeps the compositor rendering frames while
a border-carrying window is visible, so prefer slow values (for example `20`)
and leave it at `0` on battery-sensitive setups.

Only the focused window's ordinary border upgrades to the gradient. Signal
borders keep their flat colors: the focus-highlight pulse, urgent/attention
borders, and picture-in-picture windows are unaffected, as is the unfocused
border color.
