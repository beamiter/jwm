# Expose

`Alt+E` (`toggle_expose`) spreads every window currently visible on its
monitor into a live thumbnail grid, macOS Mission Control style, so you can
point at the window you mean instead of cycling to it. Windows qualify when
they are on an active tag (or sticky), not minimized or hidden, and not
[swallowed](../README.md) by a terminal; with no candidates the key does
nothing. The key is a toggle — pressing it again exits without changing
focus.

Every thumbnail carries its window's title, centered just inside the
thumbnail's top edge and ellipsized to the cell width in the system-UI
font — the same text pipeline and typography as the tab strip's and cube
overview's titles. A window without a usable name simply shows no label.
Labels fade with the grid, are pure overlay (hit-testing ignores them), and
their textures rebuild per activation and are freed on rebuild and at
compositor teardown.

On the Wayland compositor the highlighted cell's label also brightens with
the hover ease: a pre-brightened copy of the title texture — the ink mixed
a pinned 0.35 toward white, alpha untouched — draws over the normal label
at the grid's opacity scaled by the hover progress, so the title eases
brighter on the same envelope the cell lift and ring ride. Hover and the
keyboard selection are one highlight, so both get it. The X11 compositor
keeps its ring-only hover, per the standing rule that X11's existing visual
features are eased, not expanded.

Expose needs the compositor; in a deliberately non-composited session the
key reports an error rather than faking the grid.

```toml
[behavior]
expose_enabled = true   # default
expose_gap = 20.0       # pixels between thumbnails
```

Entering pre-selects the window that had focus, so `Alt+E` then `Return`
is a no-op round trip — a safe way to peek at the grid.

## Keyboard navigation

| Key | Action |
| --- | --- |
| `Left` / `Right` / `Up` / `Down` | move the highlight through the grid |
| `Return` / keypad `Enter` | focus the highlighted window and exit |
| `Esc` | exit without changing focus |
| `Alt+E` | exit without changing focus (the key is a toggle) |

Movement clamps at the grid's edges instead of wrapping, and `Down` from a
full row into an incomplete bottom row stays put — the highlight only ever
sits on a real thumbnail, so `Return` never surprises you.

## Pointer interaction

Hover and the keyboard selection are one highlight: moving the pointer
across the grid takes it over, and the arrow keys pick up from wherever it
is. Clicking a thumbnail focuses that window and exits; clicking empty
space exits without changing focus.

While expose is up, the keyboard and the pointer's buttons are grabbed, so
a stray keystroke does not leak to a window behind the grid.

## Related surfaces

- The [window switcher](window-switcher.md) (`Alt+Tab`) is the
  hold-the-modifier MRU list — faster when you know the window is recent.
- The overview (`Alt+Ctrl+Tab`, `toggle_overview`) is the Compiz-style lit
  prism with labeled faces, documented in [cube effects](cube-effects.md).
  The exposé grid labels its thumbnails the same way — titles above, one
  line each — so the two overview surfaces read alike.

## Where it lives

- `src/jwm/features/expose_plan.rs` — the enter/exit/click/escape
  decisions as pure functions, unit-tested without a display.
- `src/backend/compositor_common/expose.rs` — the grid layout and the
  highlight movement (edge clamping included), shared by both compositors,
  so a cell is exactly where the click test thinks it is.
