# Window placement

Where a new window lands is decided once, when JWM starts managing it, in
this order:

1. A transient window (a dialog with `WM_TRANSIENT_FOR`) follows its parent's
   monitor and tags and floats.
2. Everything else starts on the selected monitor, on that monitor's active
   tags: wherever the pointer is.
3. A matching `[[rules]]` entry can pin the tags, the monitor, or both.
4. The closed-placement memory, described below, moves the window to where
   the same application was last closed, unless a rule already pinned that
   part or JWM launched the window itself.

Step 2 is the right default for a window the user asked for with a
keybinding or from the launcher: they acted on this monitor, on this tag,
and expect the window here. It is the wrong default for most other windows.
A browser reopened from a shell, a viewer that an agent (`claude`, `codex`)
spawns from inside a terminal, an application's own second top-level: each
of those has a place it was last seen, and step 4 sends it back there.

```toml
[behavior]
remember_closed_placement = true   # default
```

The key is hot-reloadable and accepts `set_config`. Switching it off also
forgets everything remembered so far, so a later re-enable starts from what
the user does next rather than from stale history.

## What is remembered

When a regular top-level window is unmanaged, closed by the user or by its
process, JWM records its monitor number and tag mask under its `WM_CLASS`
(class and instance, matched exactly). The newest close of an identity
replaces the previous one. Status bars, docks, scratchpads, sticky windows,
transients and popup-like window types (dialogs, tooltips, notifications,
splash screens, utility windows) are never recorded.

The memory lives in the JWM process. It holds up to 256 identities, evicts
the oldest when full, and does not survive a restart of JWM.

## Who gets the memory

The exclusion is about who launched the window, not what it is. JWM records
the PID of every child it spawns itself: keybinding `spawn` commands, the
application launcher, scratchpads, the session menu and other shell panels.
When a new window maps, JWM walks its process ancestry and stops at the
first process it recognises:

| First recognised process on the chain | Origin | Memory |
| --- | --- | --- |
| another managed window's process | launched from a window: a terminal, an agent inside one, or the application's own second window | applied |
| a child JWM spawned | keybinding, launcher, scratchpad, shell panel | not applied |
| none (a tmux server, a daemon, an SSH session, a D-Bus service) | external | applied |

A managed window's process wins over a spawn record at every step, so an
application started inside a terminal that JWM spawned is the terminal's
doing, not JWM's. A spawn record is matched by PID and, when readable,
by the process start time, so a recycled PID cannot claim it.

Two situations give no chain to walk. A D-Bus-activated application (for
example `gnome-terminal`, whose spawned wrapper exits while a long-lived
server maps the window) reaches nothing JWM knows, and Wayland backends
currently report no PID at all. In both cases the window is blamed on the
newest JWM spawn that has not produced a window yet, provided that spawn is
at most ten seconds old, and each spawn can be blamed once. Outside that
window such a window counts as external and gets the memory.

## Where the window goes

The remembered monitor and tags are applied after rules. A rule that pins
tags keeps its tags; a rule that pins a monitor keeps its monitor; the memory
fills in only what the rule left open. If the remembered monitor is no
longer connected, the window stays on the selected monitor and only the
remembered tags apply, because tags are the user's workspaces and travel
with them across outputs.

## Focus stays where it is

A window the memory sends to another monitor or tag than the focused
window never pulls focus or the view along, whatever
`behavior.focus_follows_new_window` says for windows the user launched
here. The user was typing somewhere; a window they did not launch at the
pointer must not take that away. Instead the window is laid out where it
belongs, pre-selected on its own tag so that viewing the tag opens on it,
and marked urgent so the status bar highlights the tag and, on a visible
monitor, its border says where it went. Focusing it clears the cue. Do Not
Disturb keeps the cue quiet like every other attention request.

A window that comes back into the current view, on the selected monitor and
an active tag, is focused exactly like any other new window.

## Diagnostics

Every decision is logged under the `[closed-placement]` prefix: a close
that was remembered, a window that was launched by JWM and kept its default
placement, and a window that returned to its remembered monitor and tags
together with the origin the chain resolved to.

## Maximize

Maximize is placement too, but a temporary one: a maximized window fills its
monitor's work area, and unmaximizing returns it to where it was. The work
area is the monitor minus the status bar, docks and the tab bar. The window's
border is drawn inside that area and there are no gaps, so a window maximized
on both axes covers exactly what the monocle layout would give it.

### Who may maximize what

Requests come from three origins. The user asks through `togglemaximize`,
`snap_window maximize` (`Alt+Shift+Up`), dropping a dragged window at the top
edge, and `restore_session`. A client or a taskbar asks through EWMH
`_NET_WM_STATE`, xdg-shell, XWayland or wlr-foreign-toplevel. Adoption is a
window that already carries maximized state when JWM starts managing it,
including every window after a seamless restart. The first matching row
decides:

| Request | Window | Outcome |
| --- | --- | --- |
| only removes axes | any | applied |
| adds an axis | a dock, a fixed-size window, or one without a work area | refused |
| adds an axis | floating, including underneath fullscreen or PiP | applied |
| adds an axis | tiled underneath fullscreen or PiP | refused |
| adds an axis | tiled, float layout, any origin | promoted |
| adds an axis | tiled, tiling layout, user | promoted |
| adds an axis | tiled, tiling layout, client or adoption | refused |

A promoted window leaves the layout while it is maximized and goes back into
the slot it left when its last axis is cleared: in front of the window that
followed it, while that window is still tiled on the same monitor, so a
maximized master comes back as the master. Neighbours promoted one after the
other each go back into their own slot, in whichever order they return.
If that window closes, moves to another monitor or is moved to the front (by
zoom, by picking it in the overview, or by leaving the vstack layout with it
focused), the slot passes to the tiled window that followed it, so the
promoted window comes back where it would be had it stayed tiled: a maximized
master is still the master after the window behind it closes. A promoted
window picked in the overview or focused when leaving the vstack layout stays
out of the layout: like any floating window it only moves ahead of the other
floating windows, never in front of a tile, and passes nothing on, so it and
the neighbours promoted with it still go back into their own slots, in
whichever order they return. A promoted window goes back at the end of the
tiled windows instead when no tiled window followed it, while the window that
followed it floats for another reason (it went fullscreen, into
picture-in-picture or was floated by hand, and maximize is not holding it out
of the layout), and when it is unmaximized while it is fullscreen or in
picture-in-picture itself; it then rejoins the layout when that mode ends. A
window floating for one of those other reasons passes no slot on when it
closes or leaves. A neighbour that is still promoted keeps carrying the chain
even while it is fullscreen or in picture-in-picture, until it is unmaximized:
the window it followed still goes back into its own slot, and closing it
passes its slot on like any promoted window. A promoted window stays floating
and maximized across a switch from the float layout to a tiling one, until
something unmaximizes it.
A refused request, like one that changes nothing, leaves the window alone:
JWM republishes the current state and replies with the current geometry, a
synthetic `ConfigureNotify` on X11 and a configure on xdg-shell, so the client
never believes in a maximize that did not happen. Refusing adoption clears the
window's pre-set maximized atoms. Fixed-size windows (equal minimum and maximum
size hints) do not advertise Resize or Maximize in `_NET_WM_ALLOWED_ACTIONS`.

### Axes

Native X11 can maximize one axis at a time. Horizontal takes the work area's
x and width, vertical its y and height, and the other components keep their
values. A `_NET_WM_STATE` message naming both atoms is one request, not two.
xdg-shell, XWayland and wlr-foreign-toplevel can only express both axes and
report the window as maximized only while both are set. Dropping one of two
axes restores only that axis's position and size; the window stays maximized
on the other.

On X11 the target goes through the window's size hints: a terminal with
character-cell increments can leave a partial-cell gap, and a window with a
maximum size stays at the work area's top-left corner at that size. Wayland
has no size hints, so xdg-shell windows fill the area exactly.

### The restore rectangle

Maximizing records the window's rectangle in a restore slot of its own. No
other mode writes to it: fullscreen keeps its return rectangle, PiP and
`togglefloating` keep the floating rectangle, and minimizing keeps its parking
record, each in separate storage. Unmaximizing returns to the restore slot. A
window that already covered at least 90% of the work area in both directions
would look unchanged when unmaximized, so its restore rectangle becomes a
centered rectangle two thirds the size of the work area instead.

### Keeping it maximized

Every arrange refits maximized windows to their monitor's current work area.
A new strut, a dock or layer-shell panel, toggling the bar or the tab bar, and
an output resize therefore resize maximized windows with it. The refit is
idempotent: a window already at its target gets no configure. Moving a window
to another monitor, with `tagmon` or because its output was unplugged,
translates its restore rectangle into the target work area and maximizes it
there. A minimized or parked maximized window changes only its restore record
and is never configured on screen before it is shown again.

A maximized native X11 window's `ConfigureRequest` loses the components on its
maximized axes: x and width horizontally, y and height vertically. A request
with no geometry component left, including one that only changes the border
width, is refused with the current geometry. The free components of an
accepted request move the restore rectangle along with the window. A maximized
XWayland window cannot move or resize itself at all.

### Drag and snap

Starting to move or resize a maximized window with the modifier drag
(`movemouse`/`resizemouse`), or with a native X11 or XWayland title-bar drag
(`_NET_WM_MOVERESIZE`), unmaximizes it in place. The window keeps its current
position and size, and the drag continues from there. xdg-shell title-bar and
edge drags are not honoured at all; see [compatibility](compatibility.md). A
cancelled drag puts the window back, maximized, with its original restore
rectangle. Dropping a window at the top edge maximizes it on the monitor it
was dropped on. `snap_window maximize` toggles: it maximizes a floating
window, and pressing it again restores the window. Snapping a maximized window
to a half or a quarter unmaximizes it in place first; halves and quarters still
cover the whole monitor rather than the work area.

`togglefloating` on a maximized window unmaximizes it first. A promoted
window then simply returns to the layout; any other window is unmaximized and
then tiled. Revealing a scratchpad unmaximizes it before placing it at its
centered position.

### Fullscreen and picture-in-picture

Fullscreen and PiP suspend maximize. The window keeps its maximize state and
restore rectangle, and leaving fullscreen or PiP returns it maximized and
refit to the current work area. A maximize request while the window is
fullscreen or in PiP changes only the rectangle it will return to. A window
promoted out of a tiling layout and unmaximized while fullscreen or in PiP
rejoins the layout when that mode ends, at the end of the tiled windows rather
than in the slot it left. Fullscreen and maximized may be reported together,
as EWMH and xdg-shell allow.
`togglemaximize` and `snap_window` leave fullscreen and PiP windows alone.

### Restarts and sessions

Session snapshots do not store maximize: `restore_session` unmaximizes every
window it matches, then applies the saved placement. Across a seamless X11
restart the EWMH atoms carry the state, but a visible window's floating state
is not remembered: it floats again only when `WM_TRANSIENT_FOR`, a matching
rule or a popup-like window type such as a dialog floats it, as for any new
window. Such a window is maximized again; its previous restore rectangle is not
carried over, so it gets the centered fallback when it filled the work area. A
window floated by hand, or promoted out of a tiling layout, comes back tiled:
under a tiling layout its maximize is refused and its atoms are cleared, and
under the float layout it is promoted and maximized again. A minimized window
keeps its resting floating state through its restore snapshot, and a floating
one outside PiP also keeps its exact pre-maximize rectangle. A minimized
promoted window is saved tiled, the state it rests in, so it comes back like a
visible one.
