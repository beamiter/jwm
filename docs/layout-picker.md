# Layout picker

`Alt+Space` (`cyclelayout`) steps to the next layout, exactly as it always did,
and now brings up a strip of film while it does: one cell per layout, each
holding a line-drawn thumbnail of what that layout does with a screenful of
windows. `Alt+Shift+Space` steps the other way. The strip commits on its own a
moment after you stop, so a single tap still behaves like a plain layout switch.

On a multi-monitor desktop the strip is laid out in the selected monitor's
global viewport, its scrim covers that output only, and pointer hit-testing
uses the same offset geometry. A monitor left of or above the primary is not
treated as if it began at `(0, 0)`.

```
    ┌───────────────────────────────────────────────────────────────┐
    │  LAYOUT                                                        │
    │  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  │
    │  ░ ┌──┬─┐ ░ ┌──┬─┐ ░╔══╦══╗░ ┌────┐ ░ ┌──┬─┐ ░ ┌────┐ ░ ...  │
    │  ░ │  ├─┤ ░ │  ├┬┤ ░║  ║  ║░ ├─┬─┬┤ ░ │  │ │ ░ │    │ ░      │
    │  ░ └──┴─┘ ░ └──┴┴┘ ░╚══╩══╝░ └─┴─┴┘ ░ └──┴─┘ ░ └────┘ ░      │
    │  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░  │
    │                     |M|  Centered Master                      │
    │  ▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁                                   │
    │  ←/→ browse    Enter / click  apply    Esc  cancel            │
    └───────────────────────────────────────────────────────────────┘
```

## Browsing is live

Every step applies the layout to the real desktop behind the panel. There is no
"preview" that can disagree with the result: what the strip highlights is what
the windows are already doing, and confirming only takes the panel down.

`Esc` puts back the layout that was current when the picker opened.

## Four ways to commit

| | |
|---|---|
| `Enter` | apply the highlighted layout |
| click | apply the layout under the pointer, or the highlighted one |
| wait | after 2.6 s without interaction, the highlighted layout stands |
| `Esc` | cancel, restoring the layout the picker opened on |

Anything that moves the selection — a key, the wheel, the pointer crossing into
another cell — restarts the delay. Someone still driving the picker has not
finished choosing.

Inside the picker: `←`/`→`, `↑`/`↓`, `Tab`/`Shift+Tab` and the wheel browse;
`Space` steps forward and `Shift+Space` back, so holding `Alt` and tapping
`Space` keeps cycling the way it did before the panel existed.

## The thumbnails are the real layouts

Each cell is drawn by running that layout's own geometry function over a virtual
monitor and normalising the result, so a thumbnail cannot describe an
arrangement JWM would not produce. Fibonacci makes its real second turn; the
scrolling strip runs off both edges of its frame because that is what it does on
screen; Float shows the cascade you end up with by hand.

The window count per layout is chosen to show that layout's signature — Tatami
needs six windows before it stops looking like Grid. A rule across the top of a
thumbnail is the status bar, which is what distinguishes Monocle from
Fullscreen: both put one window over the whole screen, only Fullscreen takes the
bar with it.

## Configuration

```toml
[behavior]
layout_picker = true   # default
```

Turning it off makes `cyclelayout` switch silently again. In a deliberately
non-composited X11 session the picker temporarily leases JWM's compositor for
the panel and restores native mode when it closes. If the renderer cannot be
started, the layout still cycles silently, so the key never becomes a no-op.

`layout_picker` is bindable and dispatchable in its own right; its integer
argument is the step taken as it opens, so `0` opens the strip on the current
layout without changing anything:

```toml
[[keys]]
modifier = ["Mod1"]
key = "space"
function = "layout_picker"
argument = { Int = 0 }
```

```
jwm-tool msg layout_picker --args 0
```

## Where it lives

- `core/layout.rs` — `preview_frames`, the thumbnails, from the layout
  functions themselves.
- `jwm/features/layout_picker.rs` — selection, the auto-confirm clock.
- `jwm/layout/picker.rs` — opening, stepping, committing, cancelling.
- `backend/compositor_common/layout_strip.rs` — the strip's geometry, shared by
  both compositors and by the window manager's hit test, which is why a click
  lands on the cell it looks like it lands on.
