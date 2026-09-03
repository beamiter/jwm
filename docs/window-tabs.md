# Window tabs

A monitor whose tiling area holds more than one window gets a strip across
the top of that area, one cell per tiled window, in the monitor's tiling
order. It answers "what else is here" without cycling focus, and it doubles
as a mouse control surface: focus, close, and reorder.

The strip is drawn only when there is something to choose between — two or
more visible tiled windows. Floating windows get no cell, a fullscreen
window (or the fullscreen layout) takes the strip down with the status bar
while it owns the output, and the strip's pixels are reserved out of the
work area, so no window ever slides underneath it.

## Configuration

```toml
[behavior]
window_tabs = true      # default
tab_bar_height = 28.0   # pixels, clamped to 1..=256
```

The strip is painted in the `appearance.ui_theme` palette like every other
surface JWM draws itself, so it has no colors of its own. Titles use the
same font stack as the rest of the shell UI, with CJK and emoji fallback —
a window named in Chinese or with an emoji shows its real title, not a row
of question marks.

## Pointer interaction

| Gesture | Action |
| --- | --- |
| hover | highlights the inactive cell under the pointer at half strength |
| left-click | focuses the window (and raises it) |
| middle-click | closes the window, through the same path as `killclient` |
| left-drag past `behavior.drag_threshold_px` (default 12 px), then release | moves the window to the dropped slot in that monitor's tiling order |

The drag is a reorder, not a move: there is no live preview while it is
active, and releasing over another monitor's strip cancels rather than
carrying the window across screens. Below the threshold the gesture stays
a plain click. Opening a system-UI panel abandons an in-flight drag.

A left press focuses its window immediately, so a reorder drag always
starts from the window you just made current — matching what the layout
does with the slot it lands in.

## Where it lives

- `src/jwm/window_tabs.rs` — which windows are in the bar, the pixels the
  layout reserves for it, and what a click or drag means. Membership and
  reservation share one predicate: a strip reserved without being painted
  would strand a band of wallpaper, and one painted without being reserved
  would sit on top of the status bar.
- `src/backend/compositor_common/window_tabs.rs` — the strip's geometry
  and hit math, shared by the window manager and both compositors so the
  reserved band and the painted cells cannot drift apart. Hover state
  lives in the compositor rather than the window manager, so pointer
  motion never triggers a title-texture rebuild.
