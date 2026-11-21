# Tags overview

`Alt+O` (`toggle_tags_overview`) opens a GNOME-style grid of every tag on the
desktop at once, in a panel hosted on the selected monitor. With multiple
outputs, each output occupies one row and labels distinguish the same tag
number on different outputs. A single output uses an aspect-adaptive grid.
Each cell carries the tag's number and a line-drawn
wireframe of its windows; the tags currently on screen keep a persistent
accent frame. The key is a toggle — pressing it again closes the grid without
changing the view. The dim behind the panel fades in on the same `opened²`
envelope the other shell panels' scrims ride — instantly full when animations
are disabled — while closing stays instant, since no panel fades out.

```toml
[behavior]
tags_overview_enabled = true   # default
```

Opening highlights the current tag (the lowest active tag in a multi-tag
view). Confirming without explicitly choosing a cell closes the panel without
changing the view, preserving the complete multi-tag mask. Using keyboard/wheel navigation (even at a clamped edge), moving the pointer
to a different cell, or explicitly clicking a cell or pressing a digit chooses
a single tag instead. The overview needs the compositor; in a deliberately non-composited
session the panel starts one for its own lifetime and hands it back on close,
like the other shell cards.

## Keyboard navigation

| Key | Action |
| --- | --- |
| `Left` / `Right` / `Up` / `Down` | move the highlight through the grid |
| `Home` / `End` | jump to the first / last cell (no commit) |
| `Page Up` / `Page Down` | jump by one grid row (no commit) |
| `Return` / keypad `Enter` | close unchanged if untouched; otherwise select the highlighted tag and close |
| `1`–`9` | select that tag on the highlighted output and close |
| `Esc` | close without switching |
| `Alt+O` | close without switching (the key is a toggle) |

With multiple outputs, Left/Right move within an output row and Up/Down
move between output rows. Home/End address the whole grid, not just one output.
Movement clamps at the grid's edges instead of wrapping, and `Down` from a
full row into an incomplete bottom row stays put — the highlight only ever
sits on a real cell, so `Return` never surprises you. The digit keys work
with modifiers held too: the panel holds the keyboard grab, so the global
`Mod1+N` bindings never see the key, and the panel answers it in their place.

## Mouse

The panel grabs the pointer while it is open, so no click can fall through
to the windows underneath. A press on a cell commits nothing by itself —
the release settles what the gesture meant. The vertical wheel browses the
highlight Up/Down like the arrows (no commit); the horizontal wheel browses
Left/Right. Middle-click confirms the highlighted tag like `Return`.

- Moving the pointer over a cell highlights it. Mouse and keyboard share the
  one highlight, so you can mix them freely; the dead space between cells
  leaves the highlight where it is. The highlighted cell is drawn slightly
  lifted, and the hit-test reads it lifted too, so a press lands on the cell
  the eye sees rather than on the smaller rectangle underneath — including
  the sliver of gap the lift now covers. Where the lift overhangs a
  neighbour, the card painted last owns the pixel, which is again what is
  drawn on top.
- Clicking a cell — press and release on the same cell — jumps to that tag
  and closes. This is an explicit single-tag choice even on the initial cell.
- Middle-click anywhere on the grab confirms like `Return`: an untouched
  highlight closes unchanged; an explicit selection chooses that tag.
- Dragging a window's wireframe from its cell onto another cell moves the
  window to that tag on the target output and keeps the grid open, so several
  windows can be
  dealt out in one visit. The move replaces the window's whole tag mask with
  the target tag — the dwm `tag()` semantics of `Mod1+Shift+N`, shared with
  the same code path — so a window that sat on several tags ends up on only
  the drop target. Dropping on the dimmed desktop or the panel's dead space
  commits nothing and simply lets the press go. A sticky window's wireframe
  is the one that cannot be dragged: it draws in every cell, but tagging it
  would rewrite a mask stickiness ignores and the next `view` would put back,
  so a press on it settles as the cell's click instead of arming a drag that
  could not mean anything.
- Clicking the dimmed desktop around the panel closes without switching,
  exactly like `Esc`. The cancel answers on the press; a release that lands
  on the scrim after starting on a cell never cancels, so a misdrag stays
  harmless.
- Clicking the panel's own dead space (the title, caption and hint bands,
  the gaps between cells) does nothing — the press is swallowed.

## What the cells show

- Active tag cells can show the windows' own textures, scaled into their
  rectangles. This applies across outputs and to multiple active tags. A
  window whose texture is unavailable keeps its outline. Inactive cells stay
  wireframes rather than displaying a parked window's stale texture.
- Windows parked on another tag are drawn at the position they will return
  to, not at their off-screen parking spot.
- Sticky windows (and clients spanning every tag) draw in every cell but,
  exactly like the status bar's tag mask, do not by themselves mark a tag
  occupied. Occupied cells — any window, minimized ones included — draw their
  wireframes in the bright ink; empty cells sit back in the dim tone.
- Minimized and swallowed windows draw no outline: they own no screen real
  estate. They still count toward the cell's occupied marker.
- A tag holding an urgent window draws a small dot in
  `behavior.attention_color` at the right end of the cell's label band — the
  same token the urgent window's own border breathes in. The marker follows
  the status bar's urgent mask: an urgent window marks every tag it sits on,
  minimized and swallowed urgent windows still count, and sticky or all-tags
  windows float above the tag axis and mark nothing.
- Floating windows may hang off the work area; their wireframes are clipped
  at the cell edge rather than poking through the card.

Window changes while the grid is open (a window appears, closes, moves tags)
rebuild the cells in place; the highlight retains its output-and-tag identity.
If an output disappears or a config reload removes the selected tag, confirming
cancels instead of choosing an unrelated cell. A new explicit selection can
choose one of the remaining cells.

The grid follows the selected monitor as well. `focus_monitor` over IPC, or
an activation of a window on the other head, moves the selection without
arranging anything; the open grid notices on the next frame and rebuilds for
the new host monitor while retaining cells for every output. Changes to the
output identities or geometry cancel an in-flight pointer gesture.

## Implementation and validation boundaries

Cross-output wireframe drops are implemented: the drop resolves the target
output and tag, moves the client to that output, then replaces its tag mask.
Unit tests cover multi-output cell identity and confirmation, navigation,
and same-output drag dispatch. These tests do not establish real multi-monitor
cross-output drag acceptance or texture availability on every backend.

An explicit tag choice runs the ordinary `view` path, which emits `monitor/bar`
so occupancy subscribers see the new tag, layout knobs, and counts. Untouched
confirmation only closes the panel.

## Related surfaces

- [Expose](expose.md) (`Alt+E`) spreads the *current* tag's windows into a
  live thumbnail grid; the tags overview trades pixels for reach and shows
  every tag at once.
- The [window switcher](window-switcher.md) (`Alt+Tab`) is the
  hold-the-modifier MRU list for the windows you touched recently.
- The layout picker (`cyclelayout`) is the grid's one-dimensional sibling: a
  film strip of layout thumbnails sharing the same geometry toolkit.

## Where it lives

- `src/jwm/features/tags_overview.rs` — the snapshot (with the window id
  behind every wireframe kept parallel to it), cell building, selection walk,
  press/release gesture plan and commit as pure functions, unit-tested
  without a display.
- `src/backend/compositor_common/tags_grid.rs` — the grid geometry, the
  wireframe hit-test and the urgency badge anchor shared by both
  compositors, so a cell, its outlines and its marker are exactly where the
  WM thinks they are.

`jwm-tool health` prints compact `tabs` for tags overview operators.
