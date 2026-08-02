# UI theme

JWM draws four surfaces itself: the [debug HUD](debug-hud.md), the modal
system-UI card (launcher, keybinding viewer, lock screen), the
[notification toasts](notifications.md) and the volume/brightness OSD. They
share one palette, chosen by `appearance.ui_theme`:

```toml
[appearance]
# "material" (default), "glass", or "glass-dark"
ui_theme = "glass"
```

The setting only touches JWM's own overlays. Client windows keep their own
corner radius, shadow, border and per-window frost settings.

## The themes

### `material` — elevated surfaces

The original look, unchanged: near-opaque dark cards on the 8dp grid, lifted
off the desktop by a drop shadow, with an accent ring picked up from the
focused window's border gradient. It reads clearly against any wallpaper and
costs nothing beyond the fills it draws.

### `glass` — Apple frosted glass (毛玻璃)

The light material iOS and macOS use for folders, sheets and Control Center.
Each card samples a Kawase-blurred copy of whatever is behind it and *lifts*
it under a white veil, so the desktop's color and light carry through the
panel instead of being hidden by it.

What separates this from a plain backdrop blur is that the panel is modeled as
a **thick pane of glass**, not a translucent rectangle:

| Cue | Why |
| --- | --- |
| **Continuous corners** | The mask is a superellipse (exponent ≈ 4.2), not a circular rounded rect, so curvature eases into the straight edges. This is the most recognizable difference in silhouette between an Apple panel and a CSS `border-radius` |
| **Edge refraction** | A beveled band drags the backdrop outward along the surface normal, squeezing what lies just beyond the panel into its rim — the panel gains depth instead of reading as a decal |
| **Rim hairline + inner glow** | The bevel glows softly and ends in a specular line running the whole perimeter, brightest on the two edges aligned with the light. No accent ring is drawn: a circular ring would not follow the squircle |
| **Chroma lift and sheen** | A blur averages color toward gray, so saturation is pushed back up, and a broad diagonal sheen lights the face from the top-left |
| **White veil (≈0.5 alpha)** | Heavy enough that even over a *black* desktop the surface lands near mid-gray, keeping the dark inks above 4.5:1. That floor is what makes a light material safe on a window manager, where the content behind it is whatever the user opened |

Corner radii are larger, paddings roomier, and the shadow is wide but nearly
absent: the optics already separate the card, so a hard elevation shadow would
fight them. A small grain is dithered in to keep the wide, smooth gradients
from banding on 8-bit outputs.

### `glass-dark` — the same optics, graphite

macOS's dark vibrancy rather than iOS's light sheet: identical geometry,
refraction and rim, but a dark veil with light inks. Use it when a light UI
would clash with the rest of your desktop.

The lock card is the one exception in both glass themes. It hides the desktop
on purpose, so it draws solid.

## Requirements and fallback

Both glass themes need the compositor's blur FBO chain. JWM keeps that chain alive
whenever the theme asks for it, so `behavior.blur_enabled` does **not** have to
be on — turning it on additionally frosts individual client windows, which is a
separate feature.

If the chain cannot be created at all (a driver that refuses the FBOs, or no
GL memory for them), the panels fall back to flat translucent fills in the
glass palette's tones. Nothing errors out; the cards just stop showing the
desktop through themselves.

Cost is one full-screen blur per frame in which a panel is visible — nothing at
all on a frame with no HUD, no toast, no OSD and no launcher open. Panels drawn
back to back share a single capture.

## Switching at runtime

`appearance.ui_theme` is honored on config hot-reload and through the IPC
config setter:

```sh
jwm-tool msg set_config --args '{"key": "appearance.ui_theme", "value": "glass"}'
```

Both compositors rebuild the blur chain if the switch needs one, and
re-rasterize the panel text so the new theme's inks take effect immediately —
no restart, no relaunch of the overlays.

## Where it lives

`src/backend/compositor_common/ui_theme.rs` holds `UiTheme`, the `UiPalette`
struct and the two palettes. Both compositors read tones, metrics and glass
parameters from there, so the X11 and Wayland backends cannot drift apart; each
one only owns its GL calls (`GLASS_FRAGMENT_SHADER` in its own `shaders.rs`,
plus the backdrop capture against its own framebuffer).
