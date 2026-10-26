# Window placement

Where a new window lands is decided once, when JWM starts managing it, in
this order:

1. A transient window (a dialog with `WM_TRANSIENT_FOR`) follows its parent's
   monitor and tags and floats.
2. Everything else starts on the currently focused monitor, on that monitor's
   active tags.
3. A matching `[[rules]]` entry can pin the tags, the monitor, or both.
4. A spawn pin saved with `Alt+Ctrl+P` fixes both the output and tags for that
   application, overriding placement from `[[rules]]`.
5. If explicitly enabled, the closed-placement memory described below moves
   the window to where the same application was last closed, unless a rule
   already pinned that part, an explicit spawn pin applies, or JWM launched
   the window itself.

By default, new windows open on the currently focused tag, including windows
reopened from a terminal, by an agent (`claude`, `codex`), or by a running
application. For example, closing a browser on tag 3 and reopening it while
tag 7 is focused opens the new window on tag 7. Explicit window rules and
transient-parent placement still take precedence.

```toml
[behavior]
remember_closed_placement = false  # default
```

Omitting this key also disables the memory. Set it to `true` to opt into
returning applications to their last closed monitor and tags. Existing
configurations that explicitly set `true` keep that setting after an upgrade;
change it to `false` to use the current tag.

The key is hot-reloadable and accepts `set_config`. Switching it off also
forgets everything remembered so far — both the in-memory map and the on-disk
snapshot — so a later re-enable starts from what the user does next rather
than from stale history. While the feature is disabled the snapshot file is
deleted (or ignored); it is only loaded at startup when the feature is on.

## Pin an application's spawn location

Move a regular application window to the monitor and tag(s) where it should
open, focus it, then press **Alt+Ctrl+P**. A toast confirms the saved output
and tags. Future windows with exactly the same class and instance (the native
Wayland app identity on Wayland) open there, including windows started from
the launcher or another terminal. Window titles are not part of the match.
Press the shortcut again on a window of that identity to remove the pin;
moving it with **Alt+Shift+1…9** (or the tag overview) automatically updates
the pin to the destination. Toggling tag membership updates the saved tag
mask too. The pin stays enabled until explicitly toggled off; future spawns
use the latest destination, even before the debounced save completes.
Closing a window does not remove its pin. Transient dialogs follow their parent;
scratchpads, sticky windows, docks and popup windows cannot create pins.

The action is `toggle_spawn_placement`, also available with
`jwm-tool ipc toggle_spawn_placement`. Older key lists receive Alt+Ctrl+P
automatically when the key and action are unused; a custom binding or chord
leader takes precedence. The shortcut viewer shows the effective fallback.

Pins do not require `remember_closed_placement`. They survive JWM restarts in
`$XDG_STATE_HOME/jwm/window_preferences.json` (by default
`~/.local/state/jwm/window_preferences.json`). Output matching uses the stable
panel identity, with the monitor number as fallback. If that output and
monitor are unavailable, the pinned tags apply on the current output.
Windows placed outside the focused view request attention without switching
the user's monitor or tag. At most 256 identities can be pinned; reaching
the limit leaves existing pins intact.

## Dropdown terminal size

The terminal opened with **Alt+Shift+N** remembers its size after keyboard
resizing (`Alt+H/L`, `Alt+Shift+H/L`, `Alt+Ctrl+arrows`, or custom bindings
for `resize_horizontal` / `resize_vertical`) or a completed pointer resize.
Hiding and showing it, closing and launching it again, and restarting JWM
keep that size. Sizes are saved independently for each named scratchpad
(`term`, `music`, `calc`, etc.) in `window_preferences.json`.

The size is a fraction of the monitor's usable area: showing the terminal on
another monitor scales it to fit and centers it there. New scratchpads start
at 80% width and height. Fullscreen, maximized, PiP and hidden geometry do not
replace the remembered size. Writes are debounced for half a second while
resizing, and flushed on hide, close and exit; a failed save raises a toast.

## Dragging between outputs

A floating move or resize transfers the window to the output showing the
largest area of its border-inclusive rectangle when the drag ends. Equal
overlap keeps its current output; a drop outside every output also keeps that
ownership. This works with outputs above or left of the primary output and
with unequal output sizes.

The window adopts the destination's active tags and keeps the position chosen
by the drag. If focus changed during the drag, only the dragged window moves;
the selected window and monitor stay selected. A locked destination refuses
the transfer and pulls the window back onto its own unlocked output.

## What is remembered

With the feature enabled, when a regular top-level window is unmanaged,
closed by the user or by its process, JWM records its output identity and tag
mask under its `WM_CLASS` (class and instance, matched exactly). The output is
remembered by
`OutputIdentity.stable_key` (the connector name when EDID is absent), with
the monitor number kept as a fallback for older snapshots and for moments
when the output map has no identity. The newest close of an identity
replaces the previous one. Status bars, docks, scratchpads, sticky windows,
transients and popup-like window types (dialogs, tooltips, notifications,
splash screens, utility windows) are never recorded.

The memory holds up to 256 identities, evicts the oldest when full, and is
persisted to `closed_placement.json` (schema v2) beside `session.json` under
the XDG state directory (`$XDG_STATE_HOME/jwm/`, else `~/.local/state/jwm/`).
A restart of JWM reloads it so a browser reopened from a shell still returns
where it was last closed; v1 files without a connector still load and keep
using `monitor_num`. The JWM launch registry that attributes keybinding and
launcher windows is process-only and is not written to disk.

## Who gets the memory

When the feature is enabled, the exclusion is about who launched the window,
not what it is. JWM records the PID of every child it spawns itself: keybinding
`spawn` commands, the
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
server maps the window) reaches nothing JWM knows. In that case the window
is blamed on the newest JWM spawn that has not produced a window yet,
provided that spawn is at most ten seconds old, and each spawn can be
blamed once. Outside that window such a window counts as external and gets
the memory. On Wayland, native xdg clients expose their socket-peer PID and
XWayland windows expose `_NET_WM_PID`, so ancestry walks the same way as on
X11 when those values are present.

## Where the window goes

The remembered output and tags are applied after rules. A rule that pins
tags keeps its tags; a rule that pins a monitor keeps its monitor; the memory
fills in only what the rule left open. Apply prefers the saved connector /
stable key: it looks up the live output through `output_map` and
`enumerate_outputs`, then uses that output's current monitor number, so a
hotplug hole-fill that renumbers monitors still restores to the same panel.
If the connector is missing (v1 snapshot) or the output is gone, the saved
`monitor_num` is used; if that monitor is also gone, the window stays on the
selected monitor and only the remembered tags apply, because tags are the
user's workspaces and travel with them across outputs.

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
edge, `restore_session`, an EWMH `_NET_WM_STATE` maximize whose source
indication is pager (`data[3] == 2`), and a wlr-foreign-toplevel taskbar
`set_maximized` (mapped to the same pager origin). A client asks through EWMH
with application or unspecified source (`data[3]` 0/1), xdg-shell or
XWayland. Adoption is a window that already carries maximized state when JWM
starts managing it, including every window after a seamless restart. The first
matching row decides:

| Request | Window | Outcome |
| --- | --- | --- |
| only removes axes | any | applied |
| adds an axis | a dock, a fixed-size window, or one without a work area | refused |
| adds an axis | floating, including underneath fullscreen or PiP | applied |
| adds an axis | tiled underneath fullscreen or PiP | refused |
| adds an axis | tiled, float layout, any origin | promoted |
| adds an axis | tiled, tiling layout, user, EWMH pager, or wlr taskbar | promoted |
| adds an axis | tiled, tiling layout, client, adoption, xdg, or XWayland | refused |

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
An accepted maximize change (including promote / unmaximize / drag-cancel
reinstate) also broadcasts a `window/state` IPC event whose payload is the
same shape as one `get_windows` row (`is_maximized`, `is_maximized_vert`,
`is_maximized_horz`, geometry, floating, minimized, fullscreen, urgent, …).
The same event fires on fullscreen enter/leave, minimize/unminimize, a real
`togglefloating` flip, an `is_urgent` change (EWMH demands-attention or
ICCCM urgency), an `is_sticky` change, a real `tag` / `toggletag` mask change,
a cross-monitor `sendmon` / `tagmon`, an Above/Below stacking flip, a PiP
enter/leave, a real title or `WM_CLASS` change, a real focus change
(`is_focused` for the previous and new clients), a half/quarter float snap
(keyboard `snap_window` or mouse edge/corner drop), a free float move/resize
that actually changes the rectangle, a tiling reorder (`zoom` / `pop`,
`movestack`, scrolling column move/resize, vstack `focusstack`, scrolling
column focus), a layout-parameter change
(`incnmaster`, `setmfact`, `setgaps`, or `setlayout` / cycle / last via
`apply_layout_change`), a tag `view` / `toggleview`, and a scratchpad
hide/reveal, a `togglebar` / `setcfact` rearrange, a maximize work-area
refit, a strut / output topology rearrange for every affected client, or a
terminal swallow / unswallow (`is_swallowed` on the parent).
Refused or no-op requests do not emit it; subscribe to `window` or
`window/state` to follow flips without polling. A title change also emits
the lighter `window/title` event; focus still emits `window/focus`.
`get_windows` / `get_tree` / `window/state` also report `pid` when known,
and optional `connector` / `monitor_name` (`OutputIdentity.stable_key` /
connector name, and EDID name when known — the same fields as
`MonitorInfoIpc`) so scripts can key windows to panels across hotplug
renumbering. Both are omitted when the live output map has no identity for
that monitor. The same rows carry `is_swallowed` (a terminal hidden behind
its child), `is_on_view` (the window's tags intersect the monitor's
active tags, or it is sticky — on the current view, not merely mapped),
`is_scratchpad` / optional `scratchpad` name, `border_w`, and optional
`layout` (the monitor's current layout symbol, matching
`MonitorInfoIpc.layout`, omitted when the window has no monitor).
Chrome / size-hint flags on the same rows: `is_fixed` (size-hints refuse
resize), `is_dock`, `is_desktop`, `is_drag_floating` (user-promoted float),
`never_focus`, `skip_taskbar`, `skip_pager`, `no_decorations`,
`demands_attention` (EWMH demands-attention, distinct from `is_urgent`),
`has_strut` (contributes a `_NET_WM_STRUT(_PARTIAL)` reservation), and
`client_fact` (per-window tiled share). When size hints have been fetched,
optional `size_hints` carries `base_*` / `inc_*` / `min_*` / `max_*` /
`*_aspect`. Maximize detail: `maximize_promoted`
(`maximize_restore_tiled`), optional `maximize_restore` `{x,y,w,h}`, and
`minimized_order`. Swallow edges: optional `swallowing` (parent id this
child is swallowing) and `swallowed_by` (child id that swallowed this
terminal). Optional `transient_for` is the `WM_TRANSIENT_FOR` / xdg parent
id. Tab strip membership is `is_tabbed` plus optional `tab_index`.
Resting geometry beside live `x`/`y`/`w`/`h`: `float_rect`, `old_geometry`,
`old_border_w`, optional `hidden_restore`, optional `hidden_x` (park marker),
optional `maximize_restore_anchor`, `pip_restore_sticky`, `old_state`,
`remembers_closed_placement`, dock exclusive-zone / anchors, and
`is_status_bar`. XSync: optional `sync_counter` and `sync_value`.
`get_monitors` / `get_tree` expose the usable work area as `wx` / `wy` /
`ww` / `wh` beside the full-output `x` / `y` / `w` / `h`, plus `scale`
(fractional), `refresh_mhz` (mode refresh in millihertz; `60000` is
60 Hz), `hdr_capable`, live tiling `gap` / `m_fact` / `n_master`, and
`transform` (`wl_output` 0..=7). `get_workspaces` rows include per-tag
`gap` beside `m_fact` / `n_master`. `get_workspaces` / `get_monitors` /
`get_tree` also report `closed_placement_count`. `get_closed_placement` /
`get_cp` return the focused monitor's `closed_placement_count`.
`get_layout` / `get_gaps` / `get_nmaster` query the focused monitor's live
layout parameters; `setgaps` is an IPC command.

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
on the other. On xdg-shell, dual-axis maximize also clears the four `Tiled*`
edge states (and configure / focus paths keep them off while maximized or
fullscreen) so clients do not keep tile-edge CSD under a work-area fill.

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
to a half or a quarter unmaximizes it in place first; halves and quarters fill
the same work area maximize uses (bar, docks and tab bar excluded). Drop-zone
proximity still uses the outer monitor edges.

`togglefloating` on a maximized window unmaximizes it first. A promoted
window then simply returns to the layout; any other window is unmaximized and
then tiled. Revealing a parked or minimized scratchpad exits fullscreen and
unmaximizes it before placing it at its centered floating position on the
selected monitor and tags. If exiting fullscreen fails, the scratchpad stays
in its previous state without moving or being revealed.

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
Moving or resizing PiP preserves the rectangle it returns to when PiP ends.
A cross-output drop carries that return rectangle to the destination work
area while keeping the PiP window at the user's chosen drop position.

### Restarts and sessions

Session snapshots (v5+) store an output connector / `stable_key` on each
client entry and each monitor order list, alongside the numeric
`monitor_num`. `restore_session` resolves the connector through `output_map`
and `enumerate_outputs` to the current monitor number so hole-fill
renumbering after hotplug still restores windows (and tile order) to the
same panel; if the connector is missing (v4 and older) or the output is
gone, the saved `monitor_num` is used. Per-tag layouts in
`[[layout.tags]]` follow the same rule: JWM writes an optional `connector`
beside `monitor`, seeds by connector when the output identity is known
(including X11 `updategeom` / `createmon`, same key as Wayland
`add_monitor`), and falls back to the numeric index (with `monitor = -1`
still meaning any monitor). Older config files without `connector` keep
working. A tag's `gap`, `m_fact` and `n_master` are written only when they
differ from the configured defaults, so a later change to `gap_px`,
`m_fact` or `n_master` still reaches tags that were never tuned; a config
reload also moves every tag still on the old default to the new one, while
tags tuned with `setgaps`, `setmfact` or `incnmaster` keep their value.
Entries saved by earlier versions keep the values they recorded. Session
snapshots (v4+) also store maximize axes, the pre-maximize rectangle and
whether the window was promoted. `restore_session` unmaximizes every matched
window, applies the saved resting placement and tile order, then re-applies
maximize through `adopt_client_maximized` using the saved `promoted` bit
(same origin choice as seamless restart); the saved
restore rectangle is clamped into the destination monitor's work area so
absolute coords from another geometry cannot land off-screen after a
connector remap. Session snapshots (v6+) also store sticky
(`_NET_WM_STATE_STICKY`); `restore_session` re-applies it through
`set_client_sticky` after placement, and manage adopts a pre-map Sticky atom
the same way it adopts Above / Below / maximize across a seamless restart.
Session snapshots (v7+) also store Above / Below
(`_NET_WM_STATE_ABOVE` / `_BELOW`); `restore_session` re-applies them through
the same `apply_external_stacking_request` path client `_NET_WM_STATE`
requests use, with Above winning when both flags were somehow saved.
Session snapshots (v8+) also store minimized; `restore_session` re-applies
it through `set_client_minimized` after placement / maximize / stacking.
Session snapshots (v9+) also store fullscreen and picture-in-picture;
`restore_session` leaves any live FS/PiP first, restores resting placement,
then re-applies fullscreen (winning over PiP when both were somehow saved)
or PiP through `setfullscreen` / `set_client_pip` after stacking and before
minimized. Session snapshots (v10+) also store `client_fact`, hand-float
(`is_drag_floating`), and `no_decorations`; `restore_session` applies them
during resting placement (`client_fact` clamped to `0.25..=4.0`). Across a
seamless X11 restart the EWMH atoms carry the maximize state, and JWM's
private `_JWM_MAXIMIZE_RESTORE_V1` property carries the pre-maximize
rectangle and whether the window was promoted for a *visible* maximized
window. Minimized clients already embed that rectangle in
`_JWM_MINIMIZED_RESTORE_V1`. A visible window's plain hand-float flag is
remembered as layout membership in `_JWM_FLOATING_V1` (the float rectangle)
when the window is floating, not drag-floating, not fullscreen or PiP, and
not maximized — so a seamless X11 restart re-floats it at that rectangle
even without `WM_TRANSIENT_FOR`, a matching rule or a popup-like window
type. Transient / rule / dialog / fixed-size floats still win on manage and
do not need the property. A
promoted maximize recorded in the restore property is re-admitted under a
tiling layout (user promote); a plain tiled maximize without that flag is
still refused and its atoms cleared. Under FLOAT, adoption promotes either
way. When the restore property is present and maximize is admitted,
unmaximize returns to that exact rectangle instead of the centered fallback.
A minimized window keeps its resting floating state through its restore
snapshot, and a floating one outside PiP also keeps its exact pre-maximize
rectangle. A minimized promoted window is saved tiled, the state it rests
in, so it comes back like a visible one.

`jwm-tool health` prints compact `closed_placement` for window placement operators.

## Layout drag previews and reserved edges

Tiled drag previews use each client's decoration policy: a client with its
own decorations keeps zero compositor border, while other clients keep the
configured border. The FULLSCREEN layout previews the physical output;
other layouts preview the work area. This layout choice is separate from a
window's fullscreen state.

Scrolling insertion is calculated after excluding the dragged window and
removing columns that would become empty. A column containing other windows
remains, using its first remaining window's current coordinates rather than
reflowing the strip before the drop.

Dock reservations require overlap with the physical output, so negative
monitor coordinates are valid and parked docks outside that output do not
reserve space. Padding extends the overlap test only perpendicular to the
chosen edge. If all four observed dock reservations are zero, the configured
bar offset is used only for the part not already reserved above the work
area by a strut. Observed stacked docks retain their reservation.
