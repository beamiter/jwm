# UI theme

JWM draws these surfaces itself: the [debug HUD](debug-hud.md), the modal
system-UI card (launcher, keybinding viewer, lock screen), the
[notification toasts](notifications.md), the volume/brightness OSD, and the
[window tab strip](window-tabs.md); the overview's window labels take the
same palette as a pill chip under the title. They share one palette, chosen
by `appearance.ui_theme`, and their text all goes through one font stack
with CJK and emoji fallback:

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
| **Edge refraction** | A beveled band drags the backdrop outward along the surface normal, squeezing what lies just beyond the panel into its rim — the panel gains depth instead of reading as a decal. Two taps straddle the bent sample point, which keeps the half-resolution blur chain from shimmering under it |
| **Interior thickness** | The body of the sheet displaces its backdrop too, by a much smaller constant amount: the parallax of looking *through* a slab rather than at a film stuck over the desktop |
| **Fresnel-weighted rim + inner glow** | Reflectance is reconstructed from the bevel's own surface, so the hairline and the glow behind it brighten where the glass is grazing and fade out over the flat face. The line runs the whole perimeter, brightest on the two edges aligned with the light. No accent ring is drawn: a circular ring would not follow the squircle |
| **Real specular** | The bevel's normal rotates through the mirror angle in the corner nearest the light, so the glint lands where a pane actually catches one, instead of a flat diagonal wash across the whole face. A broad, soft lift toward the light is the diffuse half of the same illumination |
| **Chroma lift** | A blur averages color toward gray, so saturation is pushed back up |
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

### The status bar's own optics

A bar frosted through `behavior.blur_status_bar` draws on the same sheet with
different numbers, because it is ~28–40px tall and never dismissed rather than
a card the user summoned: `GlassParams::for_status_bar` shortens the bevel and
its refraction so two of them cannot meet in the middle and turn the strip into
one long lens, and raises the rim and specular, which on something this thin
are nearly all the material there is to see. Its corners come from the theme
too, not from `behavior.corner_radius` — on a bar that thin that is a stadium,
and the squircle needs a flat edge to ease its curvature out into. The tint is
lighter for a different reason: the bar is the one frosted surface whose
*client* also paints a veil (every bar in `bars/` washes 0.55 of its theme
colour over what the compositor put behind it, which is what holds its text at
contrast over an arbitrary wallpaper), so the compositor contributes only the
sheet's hue, at the shared `STATUS_BAR_GLASS_TINT_ALPHA` of 0.08. The theme's
identity — saturation, luminance, corner exponent, rim tint, grain — passes
through untouched, so an `aurora` bar still catches the same teal rim as its
panels.

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
| **Pointer hover eases in** | The quiet row preview on panels, the exposé cell lift, and the tab-strip hover fade in over ~120 ms with an ease-out curve — the same motion family as the sliding selection, so mouse and keyboard read alike. Hover-leave still clears in the same frame: nothing in the shell fades out. Exposé hit-testing keeps using the base geometry while only the drawn scale eases |
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

How deep the chrome blurs is the theme's own decision, not the client dial's:
both compositors run the palette's `blur_levels` (five of the chain's six) for
the panels, so turning `behavior.blur_strength` down for cost thins the frost on
*windows* without flattening the launcher, toasts and OSD. A status bar frosted
through `behavior.blur_status_bar` gets a depth floor for the same reason — it
is the one frosted surface that is up all the time. That depth is also why it
holds less of the previous frame's blur than a client's frost does: a deeper
chain smears further, so a video wallpaper or a player parked under the bar
would otherwise leave a dozen frames of itself ghosted across it.

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

The Shell Hub also has a **Theme** page (`T` from the Hub, or bar parameter
`6`): it lists the seven known values from `KNOWN_UI_THEMES`, marks the
current one, and applies the selection through the same in-memory
`set_config` / `apply_config_changes` path Wallpaper uses, then surgically
persists `appearance.ui_theme` to the live TOML file (comments and other
keys are preserved; a missing `[appearance]` section or key is inserted).
IPC `set_config` for the same key stays session-only — only the Hub Theme
page writes the file. There are no live previews and no motion/blur rows on
that page.

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
