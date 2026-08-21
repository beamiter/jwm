# UI theme

JWM draws four surfaces itself: the [debug HUD](debug-hud.md), the modal
system-UI card (launcher, keybinding viewer, lock screen), the
[notification toasts](notifications.md) and the volume/brightness OSD. They
share one palette, chosen by `appearance.ui_theme`:

```toml
[appearance]
# "glass" (default), "glass-dark", "aurora", "material", "nord",
# "tokyo-night", or "paper"
ui_theme = "glass"
```

The setting only touches JWM's own overlays. Client windows keep their own
corner radius, shadow, border and per-window frost settings.

The themes split into two families: three **glass** themes that sample a
blurred copy of the desktop behind each card, and four **flat** themes that
draw opaque fills, differing only in palette.

## The glass themes

### `glass` — Apple frosted glass (毛玻璃), the default

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

### `aurora` — tinted glass

The same pane again, but the veil is a deep indigo and the rim catches an
aurora teal, so the panels read as *colored* glass rather than smoked glass.
Saturation on the backdrop is pushed harder — a tinted pane is allowed to
enrich the desktop it shows — and the shadow carries a violet cast.

The lock card is the one exception in all three glass themes. It hides the
desktop on purpose, so it draws solid.

## The flat themes

None of these need the blur chain; they draw opaque fills, cast a drop
shadow, and pick up an accent ring from the focused window's border gradient.

### `material` — elevated surfaces

The original look, unchanged: near-opaque dark cards on the 8dp grid. It
reads clearly against any wallpaper and costs nothing beyond the fills it
draws.

### `nord` — Polar Night

Material's geometry retoned into the [Nord](https://www.nordtheme.com/)
palette: Polar Night surfaces under Snow Storm inks. Cooler and a step
lighter than Material.

### `tokyo-night` — indigo ground

The Tokyo Night editor theme's near-black indigo ground under its periwinkle
foreground. Darker than Nord, cooler than Material.

### `paper` — light, no blur

Warm off-white opaque cards with dark ink and a soft, slightly warm shadow —
a light UI for machines or drivers where keeping the glass themes' blur chain
alive is unwanted.

## The shell card's layout

The palette decides the tones; the card's *shape* comes from
`src/backend/compositor_common/system_ui_panel.rs`, which both compositors ask
for the same geometry so a change to the panel is one edit rather than two.
Four things it does that are worth knowing as a user:

| Behaviour | Why |
| --- | --- |
| **The card never narrows while a panel is up** | The launcher re-measures its match list on every keystroke. A card that tracked that width would breathe in and out under your typing, so the width only grows, and it grows in fixed steps rather than by single pixels. Closing the panel — or replacing it with another one — starts the width over |
| **The selection slides between rows** | The highlight springs from the row it was on to the row it is going to, so a list reads as one object you move through. It is *placed*, not slid, on the first row of a freshly opened panel and after a panel swap: sliding in from a row of a different list would be motion describing nothing |
| **The global animation switch is respected** | With `[animation] enabled = false`, `speed = "instant"`, or a zero duration, the shell card, selection, OSD, toasts and HUD snap to their target geometry and request no hidden spring frames |
| **A windowed list shows a scroll indicator** | The launcher, the notification centre, the pickers and the Hub all send the compositor a slice of a longer list. A slim capsule in the right-hand margin shows how much of the list you are looking at and where |
| **A hairline separates the list from the footer** | The footer hint names the keys that work on the panel. It is drawn one step quieter than the rows, and the rule is what keeps it from reading as one more row |
| **Long text is fitted before upload** | Every row is ellipsized against the real font metrics before its CPU/GL texture is allocated. Queries retain the caret end, and a narrow nested output wins over the normal desktop width floor |

Every theme's footer hint is held to WCAG's 3:1 contrast floor against its own
panel over the worst-case desktop, and the typed query line to the 4.5:1 body
ratio, with tests in `ui_theme.rs` that fail if a retoned palette drops under
them.

## Requirements and fallback

The glass themes need the compositor's blur FBO chain. JWM keeps that chain alive
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
struct and every palette. Both compositors read tones, metrics and glass
parameters from there, so the X11 and Wayland backends cannot drift apart; each
one only owns its GL calls (`GLASS_FRAGMENT_SHADER` in its own `shaders.rs`,
plus the backdrop capture against its own framebuffer).

The modal card's geometry lives beside it in `system_ui_panel.rs` and its
motion in `dynamic_island.rs` — both pure arithmetic with no GL, so the layout
and the springs are unit-tested without a context and neither backend can drift
from the other on where a row goes.
