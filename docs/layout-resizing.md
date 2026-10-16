# Resizing the focused window in any layout

The physical-axis resize actions work in every built-in layout, including a
workspace containing only one client. Windows stay tiled. True application
fullscreen, fixed-size windows, PiP and realized maximize keep their existing
state rules; leave that state before resizing. The `fullscreen` **layout** is
separate from true application fullscreen and supports layout resizing.

## Controls

Default bindings:

| Binding | Action |
| --- | --- |
| Alt+Control+Left / Right | Shrink / grow focused window width by 2.5% |
| Alt+Control+Up / Down | Shrink / grow height by 2.5% of usable output height |
| Alt+Control+Home | Reset the current workspace/layout resize intention |
| Alt+H / L | Shrink / grow focused window width by 2.5% |
| Alt+Shift+H / L | Grow / shrink height by 2.5% |
| Alt+J / K | Next / previous window; focus within the current scrolling column |
| Alt+Left / Right | Focus the left / right scrolling column; no-op outside scrolling |
| Alt+right mouse drag on a master | Resize its selected edge/corner in the layout |

A custom keybinding list is not rewritten on upgrade. To add these controls to
an existing configuration, bind `resize_horizontal` or `resize_vertical` with a
`Float` argument (`-0.025` or `0.025`), and `reset_layout_size` with `Int(0)`.
These actions are also available over IPC:

```sh
jwm-tool msg resize_horizontal --args '{"value":0.025}'
jwm-tool msg resize_vertical --args '{"value":-0.025}'
jwm-tool msg reset_layout_size
```

`resize_horizontal` and `resize_vertical` (keyboard or IPC) resize the focused
eligible window on both physical axes, including a tiled stack client. They do
not change focus or substitute another master as the resize anchor. Singletons,
equal-rank layouts and floating selections use the same focused-window rule.
Hidden, other-workspace, other-monitor, fullscreen, fixed-size, PiP and realized
maximize selections are ignored. Existing bindings to these actions get this
behavior without configuration changes.

Mouse admission is unchanged: eligible master drags resize within the layout;
non-master drags keep the existing floating path. Keyboard eligibility is
independent of that pointer policy.

The H/L aliases use physical width and height in every layout. Shift+H grows
height and Shift+L shrinks it, preserving the increase/decrease signs of the old
client-factor bindings. Alt+J/K still switches focus. Scrolling's former
Alt+Shift+H/L column navigation is now available on Alt+Left/Right; those two
keys do nothing in other layouts, even if that workspace previously used
scrolling.

The `setmfact` and `setcfact` actions and their IPC commands remain available
with their existing meanings, including scrolling's column-focus interpretation
of `setcfact`. Existing user bindings to them keep working. Changing an actual
layout factor or master count clears the physical resize override so that an
old size intention cannot swallow the factor command.

### Existing configuration files

Upgrading does **not** rewrite or migrate your keybinding list. The new letter
aliases and column-navigation keys are defaults for newly generated
configurations. For an existing configuration, edit the file for your session:
`~/.config/jwm/config_x11.toml` or `~/.config/jwm/config_wayland.toml` (under
`$XDG_CONFIG_HOME/jwm/` when that variable is set).

Replace the four existing Alt+H/L and Alt+Shift+H/L entries with the first four
blocks below. Do not append duplicate bindings. Add the final two blocks only
if Alt+Left/Right are unused, or choose different keys for them if your custom
configuration already uses those combinations. Leave Alt+J/K and the
Alt+Control+arrow/Home bindings unchanged.

```toml
[[keybindings.keys]]
modifier = ["Mod1"]
key = "h"
function = "resize_horizontal"
argument = -0.025

[[keybindings.keys]]
modifier = ["Mod1"]
key = "l"
function = "resize_horizontal"
argument = 0.025

[[keybindings.keys]]
modifier = ["Mod1", "Shift"]
key = "h"
function = "resize_vertical"
argument = 0.025

[[keybindings.keys]]
modifier = ["Mod1", "Shift"]
key = "l"
function = "resize_vertical"
argument = -0.025

[[keybindings.keys]]
modifier = ["Mod1"]
key = "Left"
function = "scrolling_focus_column"
argument = -1

[[keybindings.keys]]
modifier = ["Mod1"]
key = "Right"
function = "scrolling_focus_column"
argument = 1
```

Save the file, then run `jwm-tool msg reload_config` in the running session.
An invalid configuration is rejected rather than partially replacing bindings.
To keep the old factor controls instead, leave your existing four entries as
they are and use the explicit physical resize actions on other keys or over IPC.

Modifier mouse resizing now keeps eligible masters tiled. To retain the former
float-on-resize behavior for a custom mouse binding, change its action from
`resizemouse` to `resizemouse_float`; alternatively toggle that window floating
first. Non-master resize drags still use the original floating path. Client-side
resize requests continue to obey `behavior.client_moveresize`; its default
`floating-only` admission policy is unchanged.

## Geometry and layout identity

- A singleton can shrink with centered empty space and grow to its usable work
  area. This includes vstack: its initial half-size card is not a size ceiling.
- With multiple clients, increasing the focused tile's size reallocates space from
  neighbouring regions. A monotone coordinate transformation keeps each
  layout's existing rows, columns and ordering. Masters sharing a column can
  reflow together; only the selected target owns the resize intention.
- An axis with no neighbouring region, such as a single full-height master,
  shrinks the target with centered empty space on that axis. It does not shrink
  every other column's height.
- Keyboard and IPC resizing use the selected tiled client in every layout,
  regardless of its master/stack rank or `n_master`. In vstack it is the
  selected main card. The existing master-group restriction applies only to
  mouse admission in tile, fibonacci, centeredmaster, bstack, deck and threecol.
- Monocle, fullscreen, deck's stack and vstack keep their intentional overlapping
  presentation. Resizing never introduces overlap between originally separated
  outer window frames. It does not convert these layouts into a different one.
- Scrolling columns remain in their strip, and the final focused column is
  centered in the visible work area after resizing. Other columns may remain
  off-screen as they normally do in scrolling.
- Gaps and borders remain pixel-sized. Minimum/maximum, increment and aspect
  hints use the existing `resize_hints` setting. The planner fits every client
  before committing any geometry; it cannot enlarge a hinted window across a
  neighbour afterward. At a size limit it stops at the nearest feasible size.
  Tiny screens or contradictory hints can leave no feasible resize; in that
  case the original layout remains in control.

A centered edge moves at half the content-size rate, while a master's unpinned
edge moves one-to-one with the pointer. An edge pinned at the work-area boundary
cannot move outside it; dragging that edge adjusts size through the other edge.
Drag threshold, cancellation and fullscreen safety are preserved. Switching
focus during a drag never retargets the held window. Releasing after changing
workspace/layout, moving the window to another monitor, or invalidating the
resize target cancels the obsolete gesture.

## State and reset rules

Each monitor/workspace/layout has one active resize anchor and two normalized
size targets. Adjusting another eligible window explicitly replaces that anchor
starting from the new target's current dimensions. This avoids competing,
order-dependent saved preferences. Ordinary focus and repeated arrange do not
retarget or accumulate the transformation; every arrange starts with that
layout's newly computed native geometry.

Switching layouts or workspaces preserves their independent intentions.
Output size/origin changes use the current usable area and size constraints,
without stale absolute screen coordinates. Adding/removing/reordering neighbours
or changing scrolling column membership retains the same anchor's normalized
size targets while that anchor remains in the tiled topology. Each change is
replanned from fresh native geometry and may be limited by available space or
size hints. If the anchor leaves that topology, its intention is discarded;
returning later does not resurrect it. This includes hiding or moving the anchor
out of the current workspace or monitor.

A membership or grouping change cancels an active resize gesture and restores
its pre-grab intention, provided the original anchor still belongs to the new
topology. It never commits an unfinished drag or transfers its size to another
client.

These intentions survive mouse release and subsequent arrange calls. Verified
X11 restarts can also hand off committed normalized intentions and scrolling
groups under the bounded, visible-membership rules in [restart state](restart-state.md).
An unfinished drag contributes only its pre-grab intention; unavailable anchors
or outputs are not assigned to another window. Alt+Control+Home (or
`reset_layout_size`) discards the current intention and restores the native
layout geometry, including vstack's original card size.
