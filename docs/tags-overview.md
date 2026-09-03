# Tags overview

`Alt+O` (`toggle_tags_overview`) opens a GNOME-style grid of every tag on the
current monitor at once, so you can see which workspace holds what instead of
walking tags blind. Each cell carries the tag's number and a line-drawn
wireframe of its windows; the tags currently on screen keep a persistent
accent frame. The key is a toggle — pressing it again closes the grid without
changing the view.

```toml
[behavior]
tags_overview_enabled = true   # default
```

Opening pre-selects the current tag (a multi-tag view pre-selects its lowest
one), so `Alt+O` then `Return` is a no-op round trip — a safe way to peek at
the grid. The overview needs the compositor; in a deliberately non-composited
session the panel starts one for its own lifetime and hands it back on close,
like the other shell cards.

## Keyboard navigation

| Key | Action |
| --- | --- |
| `Left` / `Right` / `Up` / `Down` | move the highlight through the grid |
| `Return` / keypad `Enter` | jump to the highlighted tag and close |
| `1`–`9` | jump straight to that tag and close |
| `Esc` | close without switching |
| `Alt+O` | close without switching (the key is a toggle) |

Movement clamps at the grid's edges instead of wrapping, and `Down` from a
full row into an incomplete bottom row stays put — the highlight only ever
sits on a real cell, so `Return` never surprises you. The digit keys work
with modifiers held too: the panel holds the keyboard grab, so the global
`Mod1+N` bindings never see the key, and the panel answers it in their place.

## Mouse

The panel grabs the pointer while it is open, so no click can fall through
to the windows underneath.

- Moving the pointer over a cell highlights it. Mouse and keyboard share the
  one highlight, so you can mix them freely; the dead space between cells
  leaves the highlight where it is.
- Clicking a cell jumps to that tag and closes, exactly like `Return`.
- Clicking the dimmed desktop around the panel closes without switching,
  exactly like `Esc`.
- Clicking the panel's own dead space (the title, caption and hint bands,
  the gaps between cells) does nothing — the press is swallowed.

## What the cells show

- Windows parked on another tag are drawn at the position they will return
  to, not at their off-screen parking spot.
- Sticky windows (and clients spanning every tag) draw in every cell but,
  exactly like the status bar's tag mask, do not by themselves mark a tag
  occupied. Occupied cells — any window, minimized ones included — draw their
  wireframes in the bright ink; empty cells sit back in the dim tone.
- Minimized and swallowed windows draw no outline: they own no screen real
  estate. They still count toward the cell's occupied marker.
- Floating windows may hang off the work area; their wireframes are clipped
  at the cell edge rather than poking through the card.

Window changes while the grid is open (a window appears, closes, moves tags)
rebuild the cells in place; the highlight is a tag index, so a rebuild cannot
shift what it means. If a config reload shrinks `tags_length` under an open
grid, an out-of-range highlight commits as a cancel instead of jumping
nowhere.

## Limitations

- Only the current monitor's tags are shown; there is no cross-monitor grid.
- Cells are wireframes, not live thumbnails — the grid identifies windows by
  position and shape, not by content.

## Related surfaces

- [Expose](expose.md) (`Alt+E`) spreads the *current* tag's windows into a
  live thumbnail grid; the tags overview trades pixels for reach and shows
  every tag at once.
- The [window switcher](window-switcher.md) (`Alt+Tab`) is the
  hold-the-modifier MRU list for the windows you touched recently.
- The layout picker (`cyclelayout`) is the grid's one-dimensional sibling: a
  film strip of layout thumbnails sharing the same geometry toolkit.

## Where it lives

- `src/jwm/features/tags_overview.rs` — the snapshot, cell building,
  selection walk and commit as pure functions, unit-tested without a display.
- `src/backend/compositor_common/tags_grid.rs` — the grid geometry shared by
  both compositors, so a cell is exactly where the WM thinks it is.
