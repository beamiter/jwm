# UI theme

JWM draws these surfaces itself: the [debug HUD](debug-hud.md), the modal
system-UI card (launcher, keybinding viewer, lock screen, Shell Hub, calendar,
clipboard, Wi-Fi, session menu), the [notification toasts](notifications.md),
the volume/brightness OSD, the [window tab strip](window-tabs.md),
[exposé](expose.md) window labels, the [layout picker](layout-picker.md),
the [tags overview](tags-overview.md), the screenshot toolbar, and the
recording / capture chips. They share one palette, chosen
by `appearance.ui_theme`, and their text all goes through one font stack
with CJK and emoji fallback:

```toml
[appearance]
# "glass" (default), "glass-dark", "aurora", "material", "nord",
# "tokyo-night", or "paper"
ui_theme = "glass"
```

The setting touches JWM's own overlays and, when the X11 compositor is off,
the native window border colour and the root wallpaper. Native `BorderPixel`
is composited over the theme letterbox so unfocused rings stay visible on
the wallpaper. Urgent and picture-in-picture windows use the same ring
priority as the compositor (`attention_color`, then `pip_border_color`).
The default status bar uses a clear background. On targets that cannot
present alpha, the official bar samples the sharp root pixmap (or
`glass.wallpaper` if the bar config sets one); a readable solid background is
the fallback when neither is available. Client windows keep
their own corner radius, shadow, and per-window frost settings.

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
| **White veil (0.62–0.64 alpha)** | Holds normal-size body text and keyboard hints above the 4.5:1 text contrast floor over a black desktop, while the blurred desktop still supplies the sheet's colour |

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

### Clear or frosted status bars

All bundled bars follow one JWM setting:

```toml
[behavior]
blur_status_bar = false # default: clear background
# blur_status_bar = true # frosted glass
```

Reload the JWM config to update running bars, or set it for the current session
with `jwm-tool msg set_config --args '{"key":"behavior.blur_status_bar","value":true}'`.
The setting changes the bar background only; text, icons and individual control
pills keep their own opacity. Cairo bars give text a small contrasting outline
in clear mode. Existing explicitly configured `blur_status_bar = true` keeps
frosting. An omitted setting now selects clear. A bar's explicit
`background_opacity` remains an override for its background tint.

The shared-state wire retains its existing size/version: the optional mode is
encoded in a reserved byte, so older readers ignore it. New bars connected to
an older WM default to clear. A mode change triggers the usual theme redraw,
without restarting the bar. X11's x11rb/xcb compositors and native Wayland
already invalidate their blur membership/caches on this config reload.

True desktop transparency requires a compositor and an alpha-capable window
and presentation surface. Unsupported targets retain a readable opaque
fallback. The official `tao_glow_bar` can instead sample sharp wallpaper in
clear mode and frosted wallpaper in glass mode when it has to use an opaque
window. That fallback is wallpaper sampling, not live transparency through
other windows or animated compositor layers. Webview bars may likewise use a
configured wallpaper when their native alpha capability cannot be verified.
No global window opacity is used to fade the text along with the background.

### The status bar's own optics in frosted mode

A bar frosted through `behavior.blur_status_bar` draws on the same sheet with
different numbers, because it is ~28–40px tall and never dismissed rather than
a card the user summoned: `GlassParams::for_status_bar` shortens the bevel and
its refraction so two of them cannot meet in the middle and turn the strip into
one long lens, and raises the rim and specular, which on something this thin
are nearly all the material there is to see. JWM uses a 42 physical pixel
cross-axis thickness for its managed bars (`appearance.status_bar_height`),
plus the configured outside and inside margins. The frontend receives that
physical thickness and converts it to logical units at the output scale; a
standalone `xbar_core` bar defaults to a 42 logical pixel thickness. Its corners come from the theme
too, not from `behavior.corner_radius` — on a bar that thin that is a stadium,
and the squircle needs a flat edge to ease its curvature out into. The tint is
lighter for a different reason: the bar is the one frosted surface whose
*client* also paints a veil (in frosted mode the bar washes 0.55 of its theme
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
| **Pointer hover eases in** | The quiet row preview on panels, the exposé cell lift, and the tab-strip hover fade in over ~120 ms with an ease-out curve — the same motion family as the sliding selection, so mouse and keyboard read alike. Hover-leave still clears in the same frame. Opening docks with a spring; **closing a modal card is instant** — the compositor does not keep a ghost of the dismissed panel to fade, unlike toasts and the OSD which own their own hold+fade timers. Exposé hit-testing keeps using the base geometry while only the drawn scale eases |
| **The global animation switch is respected** | With `[animation] enabled = false`, `speed = "instant"`, or a zero duration, the shell card, selection, OSD, toasts and HUD snap to their target geometry and request no hidden spring frames |
| **A windowed list shows a scroll indicator** | The launcher, the notification centre, the pickers and the Hub all send the compositor a slice of a longer list. A slim capsule in the right-hand margin shows how much of the list you are looking at and where |
| **A hairline separates the list from the footer** | The footer hint names the keys that work on the panel. It is drawn one step quieter than the rows, and the rule is what keeps it from reading as one more row |
| **Long text is fitted before upload** | Every row is ellipsized against the real font metrics before its CPU/GL texture is allocated. Queries retain the caret end, and a narrow nested output wins over the normal desktop width floor |

### Text legibility

Footer shortcuts, notification bodies and sender names are normal-size text.
They use the same **4.5:1 minimum** as list rows and queries; being secondary
information does not make them large or decorative text. This follows the
[W3C normal-text contrast guidance](https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum.html).
The footer remains visually separate through its position and divider; toast
attribution keeps the readable secondary palette ink instead of fading it a
second time.

The glass palettes pair stronger secondary inks with slightly heavier card
veils. Dark glass search fields and action chips have their own dark tints,
so a light backdrop cannot wash out the controls. The status bar retains its
independent clear/frosted setting and tint.

The regression matrix in `ui_theme.rs` checks actual title, row, label, footer,
query, notification and action-chip pairs over **both black and white**,
including the control fill on top of its parent card. It covers encoded-sRGB
compositing and the optional scene-linear path, with or without the glass
backdrop. Dark translucent fills need more coverage in linear light; both
compositors use the same fill resolver, which includes the glass interior's
maximum face sheen and grain in its bound. The linear path targets 4.65:1 to
leave room for framebuffer rounding.

These checks cover fully visible, settled text-bearing interiors. They do
not certify transition fades, decorative bevels and glints, arbitrary custom
accent washes, HDR tone mapping, display calibration, or the whole desktop
as WCAG conformant.

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
current one, and applies the selection — `Enter` / left-click / middle click —
through the same in-memory `set_config` / `apply_config_changes` path
Wallpaper uses, then surgically persists `appearance.ui_theme` to the live
TOML file (comments and other keys are preserved; a missing `[appearance]`
section or key is inserted).
The `[appearance]` header is found by name, so a trailing comment, spaces
inside the brackets or a quoted `["appearance"]` are edited in place. The
edited text is checked before it is written: if the file uses a shape the
line edit cannot handle safely — an inline `appearance = { ... }` table or
dotted `appearance.*` keys — the theme still applies for the session, the
file is left untouched, and a `Theme: failed to persist` error is logged.
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

`jwm-tool health` prints compact `blur` for ui theme operators.
