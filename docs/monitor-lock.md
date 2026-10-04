# Locking one monitor

The session lock (`Alt+Ctrl+Escape`, `lock_screen`) owns the whole seat: it
takes the keyboard and the pointer, and every output goes opaque. That is the
wrong tool for the second screen showing something the room should not see
while you keep working on the first one.

`Alt+Ctrl+Shift+Escape` — `lock_monitor` — locks **one** monitor. It goes
behind an opaque shade; the rest of the desktop carries on exactly as it was.

## What a locked monitor gets

- **A shade.** An opaque rectangle drawn by the compositor above everything on
  that output: clients, the status bar, expose, the tags grid, the workspace
  transition. It is drawn before the frame is captured, so screenshots, screen
  recordings and the [remote viewer](remote-control.md) show the shade rather
  than what is behind it.
- **No focus.** The selection cannot move onto it — `focusmon` steps over it in
  both directions — a client on it cannot be focused, and a window that maps
  there does not take the keyboard. Locking the monitor you are on moves the
  selection to an unlocked one first.
- **No clicks.** Button presses over the shade are swallowed instead of being
  delivered to whatever is invisible underneath. The pointer itself still
  moves over it — that is how the lock key finds the shade to unlock (see
  below).
- **No rows anywhere else.** Its windows leave the `Alt+Tab` switcher, the
  launcher's window search (`/`) and the expose grid while the shade is up, so
  nothing reprints their titles — or a thumbnail of them — on a screen that is
  not locked. They come back with the shade.
- **No overview left showing it.** Expose and the overview plan their windows
  once, on entry, so a lock taken while one of them is up takes it down:
  expose always exits, because its grid spans the whole desktop and would keep
  showing the newly covered windows on the other screens, and the overview
  (`Alt+Ctrl+Tab`) closes when the windows it is cycling are on the monitor
  being locked. An overview of another monitor stays up.

The windows themselves keep running: a video behind the shade keeps playing,
and everything is where you left it when the shade comes off.

## Unlocking

**Move the pointer onto the shade and press the lock key again.** A password
card appears **on that monitor**: the session lock's card, with the same
clock, date, caps-lock row and PAM worker behind `Enter`, and it says which
monitor it belongs to.

That is the whole toggle, and the pointer is what makes it one. The key's
`-1` argument means "the monitor I am on", which is normally the selected
monitor — but the selection can never be on a locked monitor, since keeping
focus off it is what the shade is for. Over a shade the two disagree, and
there the pointer wins: it is the screen the user is pointing at. Anywhere
else the key means what it always meant, so a second screen still locks
normally while a first one is shaded.

Without a pointer to move there are two more routes, and both work whatever
is shaded:

- the control center (`Alt+F10`) carries an **Unlock Monitor N…** row;
- `jwm-tool msg unlock_monitor`, or a key bound to `lock_monitor` with an
  explicit monitor number (`argument = 0`) — naming a locked monitor opens
  its prompt rather than locking it.

While the card is up it is modal, like every other shell panel. `Esc` clears a
half-typed password; `Esc` on an idle card hands the keyboard back to the
monitors that are not locked and **leaves the shade up** — backing out of the
prompt is not an unlock. A correct password lifts that monitor's shade and
nobody else's.

The session lock outranks the prompt: if the [idle policy](idle.md) (or the
lock key) locks the session while a monitor's prompt is up, the prompt is
replaced by the session lock and the shade it was asking about stays.

## What it is not

**It is not a security boundary for the session.** The seat around it is
unlocked: whoever is at the keyboard can still use every other monitor and do
everything a logged-in user can do, including talking to JWM over its IPC
socket. What the shade does is hide one screen's contents from the room. Use
`lock_screen` to lock the session.

The shades live in memory only: restarting JWM (or a crash) brings every
monitor back uncovered.

Two consequences follow from that, and both are deliberate:

- **At least one monitor stays unlocked.** Locking the last unlocked monitor
  is refused with a message naming `lock_screen`, because a shade over every
  output is a black desktop with a live keyboard behind it — worse than either
  thing on its own. A single-output session is refused for the same reason.
- **It needs the compositor.** Nothing draws the shade without one, and a lock
  the user cannot see is a monitor they believe is covered and is not. Locking
  is refused when compositing is off; while any monitor is locked
  `togglecompositor` refuses to switch compositing off, and a config reload
  that asks for it defers until the last shade comes down. When compositing
  was off and a shell panel switched it on for itself, a monitor locked while
  that panel was open keeps the compositor on after the panel closes; the
  compositor is handed back when the last shade comes down, by an unlock or
  by a display change (below).

## Display changes

A lock remembers the rectangle its monitor had when it was locked. If that
output moves, resizes or goes away, the lock is dropped — monitor numbers are
positional, and a lock that survived a display change on its number alone
would shade whichever screen inherited the number while leaving the one you
locked on show. The drop is logged and broadcast as a `monitor/lock` event
with `"reason": "output_changed"`; re-locking is one key away.

"One monitor always stays unlocked" survives the change too. If the output
you were working on is the one that goes away, the oldest lock gives way
(`"reason": "last_unlocked_output"`) rather than leaving a desktop shaded end
to end with nowhere to draw the prompt that would lift any of it, and the
selection moves to the screen that is now clear.

A display change that drops the last lock ends the lock's hold on a compositor
a panel switched on, exactly as unlocking does, so compositing does not stay
on with no shade left to draw. If a panel is still open at that moment it may
be drawing on that same compositor, so it keeps it, and the panel's own close
hands it back.

## Control

```bash
jwm-tool msg lock_monitor                          # the monitor in use
jwm-tool msg lock_monitor --args '1'               # monitor 1
jwm-tool msg unlock_monitor                        # the most recently locked one
jwm-tool msg unlock_monitor --args '{"value":1}'   # monitor 1
jwm-tool msg get_monitors                          # each monitor's "locked" flag
```

`lock_monitor` on a monitor that is already locked opens its unlock prompt, so
one binding covers both directions:

```toml
[[key_bindings.keys]]
modifier = ["Mod1", "Control", "Shift"]
key = "Escape"
function = "lock_monitor"
argument = -1             # -1 = the monitor in use, or the shade under the
                          #      pointer; 0, 1, … name one
```

A config file that already carries a `[[keybindings.keys]]` list is the whole
key table — the built-in defaults do not merge into it — so an existing
config gains this chord through the same back-fill the audio recorder, the
display layout and the tags overview use: `lock_monitor` is bound to
`Alt+Ctrl+Shift+Escape` at load time unless the file binds the action itself
or already spends that chord. The log line says which happened.

Every lock and unlock broadcasts a `monitor/lock` IPC event
(`{"monitor": 1, "locked": true}`), and `get_monitors` / `get_tree` carry a
`locked` flag, an optional `connector` (`OutputIdentity.stable_key` /
connector name), an optional `name` (backend / wl_output name), an
optional `monitor_name` (EDID name when known), and optional EDID
`vendor` / `product_code` / `serial_number` / `monitor_serial` per
monitor, so a status bar can show which screens are down and key or label
panels the same way session restore does. Each monitor row also reports the
work area as `wx` / `wy` / `ww` / `wh` (status bar, strut docks, and the
window tab bar excluded) beside the full-output `x` / `y` / `w` / `h`,
plus `scale` (fractional) and `refresh_mhz` (mode refresh in millihertz),
`hdr_capable` when the live output advertised HDR, and `vrr_supported` /
`vrr_enabled` (status mirror of `get_wayland_status` VRR caps). Each
monitor row also reports the live tiling `gap` (pixels), `m_fact`,
`n_master`, and `transform` (`wl_output` 0..=7 from the live output; `0`
when unknown). Optional `hdr_metadata`, physical size (`physical_*_mm`),
preferred mode, `vrr_min_hz` / `vrr_max_hz`, `prev_layout`, `show_bar`,
`bar_visible` (the bar window actually occupying the output; false during
client fullscreen even when `show_bar` is still the tag preference),
`has_visible_fullscreen` (a non-hidden fullscreen client on the current
view),
strut reservations, `selected_id`, dual-tagset `sel_tags` /
`previous_tags`, pertag `cur_tag` / `prev_tag`, and optional
`output_connector` (raw `OutputIdentity.connector`, which may differ from
`connector` when `stable_key` is EDID-derived) round out the row.
`get_workspaces` rows carry the same per-tag `gap` beside
`m_fact` / `n_master`, plus `show_bar` / `prev_layout` / `selected_id` and
per-tag `minimized_count` / `floating_count` / `sticky_count`. Focused-monitor
convenience queries `get_layout`
(layout symbol + `m_fact` + `n_master` + `gap`), `get_gaps`, `get_mfact`,
`get_nmaster`, `get_show_bar`, `get_prev_layout`, and `get_closed_placement`
return the selected monitor's live values (optional `connector` when known).
`get_show_bar`
also reports `bar_visible`, `has_visible_fullscreen` and
`owns_output_count` (`get_bar` /
`get_bar_visible` / `get_owns_output` / `get_visible_fullscreen` /
`get_vf` alias the same snapshot). A
per-monitor show-bar snapshot is available for any output, not only the
focused one (JSON keys `monitor` / `tag` / `show_bar` / `bar_visible` /
`has_visible_fullscreen` / `owns_output_count` / optional `connector`).
They also include `layout` (`lt_symbol`). They also include `gap`.
They also include `mfact`.
They also include `nmaster`.
They also include `prev_tag`.
They also include `selected_id`.
They also include `sel_tags`.
They also include `previous_tags`.
They also include `active_tags`.
They also include `window_count`.
They also include `on_view_count`.
They also include `floating_count`.
They also include `minimized_count`.
They also include `sticky_count`.
They also include `urgent_count`.
They also include `fullscreen_count`.
They also include `pip_count`.
They also include `maximized_count`.
They also include `above_count`.
They also include `below_count`.
They also include `scratchpad_count`.
They also include `tabbed_count`.
They also include `dock_count`.
They also include `desktop_count`.
They also include `never_focus_count`.
They also include `skip_taskbar_count`.
They also include `skip_pager_count`.
They also include `no_decorations_count`.
They also include `drag_float_count`.
They also include `swallowed_count`.
They also include `demands_attention_count`.
They also include `fixed_count`.
They also include `strut_count`.
They also include `maximize_promoted_count`.
They also include `status_bar_count`.
They also include `prev_layout`.
They also include `closed_placement_count`.
Tag client counts accumulate `closed_placement`.
Workspace rows report `closed_placement_count`.
Monitor rows report `closed_placement_count`.
Tree nodes report `closed_placement_count`.
Portal monitor rows deserialize `closed_placement_count`.
Portal window rows deserialize `remembers_closed_placement`.
`incnmaster` emits `monitor/bar`.
Scrolling column moves emit `monitor/bar`.
Scrolling consume emits `monitor/bar`.
Scrolling expel emits `monitor/bar`.
Output geometry changes emit `monitor/bar`.
WM_CLASS status-bar identity flips emit `monitor/bar`.
Title status-bar identity flips emit `monitor/bar`.
Config reload emits `monitor/bar`.
Scrolling column focus emits `monitor/bar`.
`movestack` emits `monitor/bar`.
Closed-placement settle emits `monitor/bar`.
`pop` emits `monitor/bar`.
Transient-for floating emits `monitor/bar`.
Hiding a scratchpad emits `monitor/bar`.
Showing a scratchpad emits `monitor/bar`.
`_NET_WM_STATE_SKIP_TASKBAR` emits `monitor/bar`.
`_NET_WM_STATE_SKIP_PAGER` emits `monitor/bar`.
Reconciling decoration hints emits `monitor/bar`.
Drag-start floating emits `monitor/bar`.
Cancelling a pointer drag emits `monitor/bar`.
Size-hint fixed-state changes emit `monitor/bar`.
Window-type dock/desktop updates emit `monitor/bar`.
WM_HINTS never-focus changes emit `monitor/bar`.
Drag-snap drops emit `monitor/bar`.
`setcfact` emits `monitor/bar`.
External strut property updates emit `monitor/bar`.
External strut rehosts emit `monitor/bar`.
Window-tab reorders emit `monitor/bar`.
Keep-above and keep-below changes emit `monitor/bar`.
Toggling maximize emits `monitor/bar`.
Unmaximize-in-place emits `monitor/bar`.
Reinstating a maximize snapshot emits `monitor/bar`.
Toggling picture-in-picture emits `monitor/bar`.
Urgency changes emit `monitor/bar`.
Demands-attention changes emit `monitor/bar`.
Toggling sticky emits `monitor/bar` even when the client was not fullscreen.
`togglefloating` emits `monitor/bar`.
Minimizing or restoring a client emits `monitor/bar` even when it was not fullscreen.
Managing a client emits `monitor/bar`.
Unmanaging a client emits `monitor/bar` even when it was not fullscreen.
`zoom` emits `monitor/bar`.
`focusstack` emits `monitor/bar`.
Scrolling in-column focus emits `monitor/bar`.
`focus_none` emits `monitor/bar`.
`focus_window` emits `monitor/bar`.
`setnmaster` emits `monitor/bar`.
Scrolling `setmfact` (column width) emits `monitor/bar`.
`setmfact` emits `monitor/bar`.
`setgaps` emits `monitor/bar`. Layout changes emit
`monitor/bar` after `layout/set`. Client fullscreen flips emit `monitor/bar` with that
snapshot. `togglebar` emits the same event after arrange (shared
`monitor/bar` helper). Tag `view` emits it after arrange as well, as does `toggleview`. Fullscreen layout enter/leave emits it too, as does minimizing a
fullscreen client. Closing one emits it after `window/close`. Swallowing a terminal
emits it for that monitor. Sending a fullscreen client to another
output emits it on both monitors. `focusmon` emits it for the newly
focused output. Pointer crossings emit `monitor/focus` and
`monitor/bar` as well. Session restore emits `monitor/bar` for every
output. External strut apply/remove does the same. Output hotplug
does too. Subscribe topic `bar` aliases `monitor/bar` (occupancy
only). `jwm-tool msg --subscribe bar` is the occupancy-only listener
(README and `tools/README.md` examples store it as `monitor/bar`).
`get_cfact` /
`get_selected` / `get_focused_window` twin the focused client. IPC `setgaps` /
`set_gaps`, `setmfact` / `set_mfact`, and `setcfact` / `set_cfact` adjust the
focused monitor or client. Each monitor also reports `tab_bar_reserved` (pixels
for the window tab strip, or `0`). `get_tab_bar` returns the focused
monitor's strip membership; `get_system_ui` reports the open shell panel
`kind`. `get_notifications` adds `center_open` / `selected_id`.
`get_workspaces` / `get_windows` /
`get_tree` / `window/state` expose the same optional `connector` and
`monitor_name` on each workspace and window row (omitted when unknown).
`get_workspaces` also reports `is_urgent` per tag (true when any non-sticky
client on that tag demands attention), matching the status-bar urgent mask,
plus `is_occupied` (status-bar occupied mask) and `has_fullscreen` /
`has_visible_fullscreen` / `owns_output_count` (monitor rows count with
the same visibility as the status-bar hide, swallowed terminals
excluded; workspace rows report that
count only on the active tag; tag counts skip swallowed terminals;
workspace `owns_output_count` is zero off-view).
`get_tree` uses the same hide-bar `owns_output_count` on each monitor node
(documented on the tree row type).
`get_windows` / `get_tree` / `window/state` also report `is_swallowed` and
`is_on_view` (tag intersection with the monitor's active tags, or sticky),
plus chrome / size-hint fields (`is_fixed`, `is_dock`, `is_desktop`,
`is_drag_floating`, `never_focus`, `skip_taskbar`, `skip_pager`,
`no_decorations`, `demands_attention`, `has_strut`, `client_fact`) and
`owns_output` when a visible fullscreen client currently covers the
output (swallowed terminals never own it).
`get_status` nests compact `resources` / `connectivity` / `power` / `media` /
`notifications` / `blur` / `hdr` / `capture` / `idle` / `recording` /
`audio_recording` / `clipboard` / `waterlily` / `night_light` / `magnifier` /
`peek` / `expose` / `gesture` / `wayland` / `dnd` / `session_lock` summaries
beside feature flags for shell pickers, monitor lock, and the debug HUD.
`get_status.show_bar` is the same occupancy snapshot as `get_show_bar`
(including `get_visible_fullscreen` / `get_vf`), plus `tag` / `prev_tag` /
`layout` / `gap` / `mfact` / `nmaster` / `selected_id` / `sel_tags` /
`previous_tags` / `active_tags` / `window_count` / `on_view_count` /
`floating_count` / `minimized_count`.
`get_status.show_bar` rustdoc also names `sticky_count`.
`get_status.show_bar` rustdoc also names `urgent_count`.
`get_status.show_bar` rustdoc also names `fullscreen_count`.
`get_status.show_bar` rustdoc also names `pip_count`.
`get_status.show_bar` rustdoc also names `maximized_count`.
`get_status.show_bar` rustdoc also names `above_count`.
`get_status.show_bar` rustdoc also names `below_count`.
`get_status.show_bar` rustdoc also names `scratchpad_count`.
`get_status.show_bar` rustdoc also names `tabbed_count`.
`get_status.show_bar` rustdoc also names `dock_count`.
`get_status.show_bar` rustdoc also names `desktop_count`.
`get_status.show_bar` rustdoc also names `never_focus_count`.
`get_status.show_bar` rustdoc also names `skip_taskbar_count`.
`get_status.show_bar` rustdoc also names `skip_pager_count`.
`get_status.show_bar` rustdoc also names `no_decorations_count`.
`get_status.show_bar` rustdoc also names `drag_float_count`.
`get_status.show_bar` rustdoc also names `swallowed_count`.
`get_status.show_bar` rustdoc also names `demands_attention_count`.
`get_status.show_bar` rustdoc also names `fixed_count`.
`get_status.show_bar` rustdoc also names `strut_count`.
`get_status.show_bar` rustdoc also names `maximize_promoted_count`.
`get_status.show_bar` rustdoc also names `status_bar_count`.
`get_status.show_bar` rustdoc also names `prev_layout`.
`get_status.show_bar` rustdoc also names `closed_placement_count`.
`jwm-tool msg` help lists `get_show_bar`, `get_vf`, `get_owns_output`
and `get_visible_fullscreen` / `get_bar_visible` / `get_bar`.
`jwm-tool msg` after-help examples include `get_vf` and `get_show_bar`.
`jwm-tool capabilities` text lists `bar->monitor/bar` and occupancy
query aliases of `get_show_bar`.
`jwm-tool health` prints focused-bar occupancy when nested, including
`has_visible_fullscreen`. The line appends `connector` when known and includes the monitor
number and the current `tag` and `layout` and `gap` and `mfact` and `nmaster` and `prev_tag` and `selected_id` and `sel_tags` and `previous_tags` and `active_tags` and `window_count` and `on_view_count` and `floating_count` and `minimized_count` and `sticky_count` and `urgent_count` and `fullscreen_count` and `pip_count` and `maximized_count` and `above_count` and `below_count` and `scratchpad_count` and `tabbed_count` and `dock_count` and `desktop_count` and `never_focus_count` and `skip_taskbar_count` and `skip_pager_count` and `no_decorations_count` and `drag_float_count` and `swallowed_count` and `demands_attention_count` and `fixed_count` and `strut_count` and `maximize_promoted_count` and `status_bar_count` and `closed_placement_count` and `prev_layout`.
README control examples include `get_show_bar` and `get_vf`, as does
`tools/README.md`. README names occupancy `tag` / `prev_tag` / `layout` /
`gap` / `mfact` / `nmaster`. README occupancy JSON also names
`selected_id`. README occupancy JSON also names `sel_tags` /
`previous_tags` / `active_tags`. README occupancy JSON also names
`window_count`. README occupancy JSON also names `on_view_count`.
README occupancy JSON also names `floating_count`.
README occupancy JSON also names `minimized_count`.
README occupancy JSON also names `sticky_count`.
README occupancy JSON also names `urgent_count`.
README occupancy JSON also names `fullscreen_count`.
README occupancy JSON also names `pip_count`.
README occupancy JSON also names `maximized_count`.
README occupancy JSON also names `above_count`.
README occupancy JSON also names `below_count`.
README occupancy JSON also names `scratchpad_count`.
README occupancy JSON also names `tabbed_count`.
README occupancy JSON also names `dock_count`.
README occupancy JSON also names `desktop_count`.
README occupancy JSON also names `never_focus_count`.
README occupancy JSON also names `skip_taskbar_count`.
README occupancy JSON also names `skip_pager_count`.
README occupancy JSON also names `no_decorations_count`.
README occupancy JSON also names `drag_float_count`.
README occupancy JSON also names `swallowed_count`.
README occupancy JSON also names `demands_attention_count`.
README occupancy JSON also names `fixed_count`.
README occupancy JSON also names `strut_count`.
README occupancy JSON also names `maximize_promoted_count`.
README occupancy JSON also names `status_bar_count`.
README occupancy JSON also names `prev_layout`.
README occupancy JSON also names `closed_placement_count`.
`tools/README.md` also names `minimized_count`.
`tools/README.md` also names `sticky_count`.
`tools/README.md` also names `urgent_count`.
`tools/README.md` also names `fullscreen_count`.
`tools/README.md` also names `pip_count`.
`tools/README.md` also names `maximized_count`.
`tools/README.md` also names `above_count`.
`tools/README.md` also names `below_count`.
`tools/README.md` also names `scratchpad_count`.
`tools/README.md` also names `tabbed_count`.
`tools/README.md` also names `dock_count`.
`tools/README.md` also names `desktop_count`.
`tools/README.md` also names `never_focus_count`.
`tools/README.md` also names `skip_taskbar_count`.
`tools/README.md` also names `skip_pager_count`.
`tools/README.md` also names `no_decorations_count`.
`tools/README.md` also names `drag_float_count`.
`tools/README.md` also names `swallowed_count`.
`tools/README.md` also names `demands_attention_count`.
`tools/README.md` also names `fixed_count`.
`tools/README.md` also names `strut_count`.
`tools/README.md` also names `maximize_promoted_count`.
`tools/README.md` also names `status_bar_count`.
`tools/README.md` also names `prev_layout`.
`tools/README.md` also names `closed_placement_count`.
`tools/README.md` also names `floating_count`.
`tools/README.md` also names `on_view_count`.
`tools/README.md` also names `window_count`.
`tools/README.md` names occupancy keys, including `selected_id`.
`tools/README.md` also names `sel_tags` /
`previous_tags` / `active_tags`. Window-tabs docs name occupancy `tag` / `layout` / `gap` / `mfact` /
`nmaster` / `selected_id`. Window-tabs docs also name `sel_tags` /
`previous_tags` / `active_tags`.
Window-tabs docs also name `window_count`.
Window-tabs docs also name `on_view_count`.
Window-tabs docs also name `floating_count`.
Window-tabs docs also name `minimized_count`.
Window-tabs docs also name `sticky_count`.
Window-tabs docs also name `urgent_count`.
Window-tabs docs also name `fullscreen_count`.
Window-tabs docs also name `pip_count`.
Window-tabs docs also name `maximized_count`.
Window-tabs docs also name `above_count`.
Window-tabs docs also name `below_count`.
Window-tabs docs also name `scratchpad_count`.
Window-tabs docs also name `tabbed_count`.
Window-tabs docs also name `dock_count`.
Window-tabs docs also name `desktop_count`.
Window-tabs docs also name `never_focus_count`.
Window-tabs docs also name `skip_taskbar_count`.
Window-tabs docs also name `skip_pager_count`.
Window-tabs docs also name `no_decorations_count`.
Window-tabs docs also name `drag_float_count`.
Window-tabs docs also name `swallowed_count`.
Window-tabs docs also name `demands_attention_count`.
Window-tabs docs also name `fixed_count`.
Window-tabs docs also name `strut_count`.
Window-tabs docs also name `maximize_promoted_count`.
Window-tabs docs also name `status_bar_count`.
Window-tabs docs also name `prev_layout`.
Window-tabs docs also name `closed_placement_count`.
`toggletag` on a fullscreen client emits `monitor/bar`, as does `tag`
and sticky.
`get_workspaces` / `get_tree` also report pip / maximized / above / below /
fixed (and tree scratchpad / tabbed) counts; monitors report
`window_count` / floating / minimized / sticky counts; windows report
optional `stack_index`.
`get_workspaces` also reports `closed_placement_count`.
`get_monitors` also reports `closed_placement_count`.
`get_tree` also reports `closed_placement_count`.
Overview confirm emits `monitor/bar`.
Expose exit emits `monitor/bar`.
Window-switcher commit emits `monitor/bar`.
Window-placement docs name `closed_placement_count`.
WM setup emits `monitor/bar`.
`focus_tab` emits `monitor/bar`.
Expose docs name `monitor/bar` on focused exit.
Tags-overview docs name `monitor/bar` on confirm.
Window-switcher docs name `monitor/bar` on commit.
Layout-picker docs name `monitor/bar` on live apply.
`get_closed_placement` / `get_cp` return the focused monitor's closed-placement count.
Capabilities list `get_closed_placement` and `get_cp`.
`get_status` nests compact `closed_placement` beside `prev_layout`.
`jwm-tool msg` help lists `get_closed_placement` and `get_cp`.
`jwm-tool msg` after-help examples include `get_cp`.
README control examples include `get_cp`.
`tools/README.md` control examples include `get_cp`.
Compatibility docs name `get_cp` among short query aliases.
Window-placement docs name `get_closed_placement` / `get_cp`.
Focused layout-knob query docs name `get_closed_placement`.
Launcher window activation emits `monitor/bar`.
Launcher docs name `monitor/bar` on window activation.
`_NET_ACTIVE_WINDOW` activation emits `monitor/bar`.
Foreign-toplevel activate emits `monitor/bar`.
Cube-effects docs name `monitor/bar` on overview confirm.
`jwm-tool capabilities` text lists `get_cp -> get_closed_placement`.
`jwm-tool health` prints compact `closed_placement` beside occupancy.
`get_status.closed_placement` rustdoc names `get_closed_placement` / `get_cp`.
Minimized-dock docs name `monitor/bar` on restore.
README health text names compact `closed_placement`.
`tools/README.md` health text names compact `closed_placement`.
Window-tabs docs name `get_closed_placement` / `get_cp`.
`jwm-tool health` prints compact `prev_layout` beside occupancy.
README health text names compact `prev_layout`.
`tools/README.md` health text names compact `prev_layout`.
`jwm-tool capabilities` text lists `get_pl -> get_prev_layout`.
Window-tabs docs name `monitor/bar` on focus / reorder.
`jwm-tool msg` help lists `get_prev_layout` and `get_pl`.
`jwm-tool msg` after-help examples include `get_pl`.
README control examples include `get_pl`.
`tools/README.md` control examples include `get_pl`.
`jwm-tool health` prints compact `cfact` beside occupancy.
`jwm-tool health` prints compact `gaps` beside occupancy.
`jwm-tool health` prints compact `mfact` beside occupancy.
`jwm-tool health` prints compact `nmaster` beside occupancy.
README health text names compact `cfact` / `gaps` / `mfact` / `nmaster`.
`tools/README.md` health text names compact `cfact` / `gaps` / `mfact` / `nmaster`.
`jwm-tool capabilities` text lists `get_cf -> get_cfact`.
`jwm-tool capabilities` text lists `get_gap -> get_gaps`.
`jwm-tool capabilities` text lists `get_mf -> get_mfact`.
`jwm-tool capabilities` text lists `get_nm -> get_nmaster`.
`jwm-tool health` prints compact `layout` beside occupancy.
`jwm-tool capabilities` text lists `get_lt -> get_layout`.
README health text names compact `layout`.
`tools/README.md` health text names compact `layout`.
`jwm-tool health` prints compact `tabs` beside occupancy.
README health text names compact `tabs`.
`tools/README.md` health text names compact `tabs`.
`jwm-tool capabilities` text lists `get_tab,get_tabs -> get_tab_bar`.
`jwm-tool health` prints compact `selected` beside occupancy.
README health text names compact `selected`.
`tools/README.md` health text names compact `selected`.
`jwm-tool capabilities` text lists `get_sel -> get_selected`.
`jwm-tool health` prints compact `struts` beside occupancy.
README health text names compact `struts`.
`tools/README.md` health text names compact `struts`.
`jwm-tool capabilities` text lists `get_strut -> get_struts`.
`jwm-tool health` prints compact `scratchpads` beside occupancy.
README health text names compact `scratchpads`.
`tools/README.md` health text names compact `scratchpads`.
`jwm-tool capabilities` text lists `get_pads,get_scratch -> get_scratchpads`.
`jwm-tool health` prints compact `focused` beside occupancy.
README health text names compact `focused`.
`tools/README.md` health text names compact `focused`.
`jwm-tool capabilities` text lists `get_fw -> get_focused_window`.
`jwm-tool health` prints compact `monitors` beside occupancy.
README health text names compact `monitors`.
`tools/README.md` health text names compact `monitors`.
`jwm-tool capabilities` text lists `get_mons,get_outputs -> get_monitors`.
`jwm-tool health` prints compact `workspaces` beside occupancy.
README health text names compact `workspaces`.
`tools/README.md` health text names compact `workspaces`.
`jwm-tool capabilities` text lists `get_ws,get_tags,get_desktops -> get_workspaces`.
`jwm-tool health` prints compact `windows` beside occupancy.
README health text names compact `windows`.
`tools/README.md` health text names compact `windows`.
`jwm-tool capabilities` text lists `get_wins,get_clients,get_cli -> get_windows`.
`jwm-tool health` prints compact `tree` beside occupancy.
README health text names compact `tree`.
`tools/README.md` health text names compact `tree`.
`jwm-tool health` prints compact `effects` beside occupancy.
README health text names compact `effects`.
`tools/README.md` health text names compact `effects`.
`jwm-tool capabilities` text lists `get_fx,get_effects -> get_effect_status`.
`jwm-tool health` prints compact `mic` beside occupancy.
README health text names compact `mic`.
`tools/README.md` health text names compact `mic`.
`jwm-tool capabilities` text lists `get_mic,get_mute -> get_mic_mute`.
`jwm-tool health` prints compact `bench` beside occupancy.
README health text names compact `bench`.
`tools/README.md` health text names compact `bench`.
`jwm-tool capabilities` text lists `get_bench,get_bm -> benchmark_report`.
`jwm-tool health` prints compact `floating` beside occupancy.
README health text names compact `floating`.
`tools/README.md` health text names compact `floating`.
`jwm-tool health` prints compact `minimized` beside occupancy.
README health text names compact `minimized`.
`tools/README.md` health text names compact `minimized`.
`jwm-tool health` prints compact `sticky` beside occupancy.
README health text names compact `sticky`.
`tools/README.md` health text names compact `sticky`.
`jwm-tool health` prints compact `urgent` beside occupancy.
README health text names compact `urgent`.
`tools/README.md` health text names compact `urgent`.
`jwm-tool health` prints compact `fullscreen` beside occupancy.
README health text names compact `fullscreen`.
`tools/README.md` health text names compact `fullscreen`.
`jwm-tool health` prints compact `pip` beside occupancy.
README health text names compact `pip`.
`tools/README.md` health text names compact `pip`.
`jwm-tool health` prints compact `notifications` beside occupancy.
README health text names compact `notifications`.
`tools/README.md` health text names compact `notifications`.
`jwm-tool health` prints compact `blur` beside occupancy.
README health text names compact `blur`.
`tools/README.md` health text names compact `blur`.
`jwm-tool health` prints compact `hdr` beside occupancy.
README health text names compact `hdr`.
`tools/README.md` health text names compact `hdr`.
`jwm-tool health` prints compact `dnd` beside occupancy.
README health text names compact `dnd`.
`tools/README.md` health text names compact `dnd`.
`jwm-tool health` prints compact `system_ui` beside occupancy.
README health text names compact `system_ui`.
`tools/README.md` health text names compact `system_ui`.
`jwm-tool capabilities` text lists `get_ui -> get_system_ui`.
`jwm-tool health` prints compact `idle` beside occupancy.
README health text names compact `idle`.
`tools/README.md` health text names compact `idle`.
`jwm-tool health` prints compact `clipboard` beside occupancy.
README health text names compact `clipboard`.
`tools/README.md` health text names compact `clipboard`.
`jwm-tool health` prints compact `session_lock` beside occupancy.
README health text names compact `session_lock`.
`tools/README.md` health text names compact `session_lock`.
`jwm-tool capabilities` text lists `get_lock,get_sess -> get_session_lock`.
`jwm-tool health` prints compact `tearing` beside occupancy.
README health text names compact `tearing`.
`tools/README.md` health text names compact `tearing`.
`jwm-tool capabilities` text lists `get_tearing,get_th -> get_tearing_hints`.
`jwm-tool health` prints compact `xwayland` beside occupancy.
README health text names compact `xwayland`.
`tools/README.md` health text names compact `xwayland`.
`jwm-tool capabilities` text lists `get_xwayland,get_xw -> get_xwayland_status`.
`jwm-tool health` prints compact `scrolling` beside occupancy.
README health text names compact `scrolling`.
`tools/README.md` health text names compact `scrolling`.
`jwm-tool capabilities` text lists `get_scrolling -> get_scrolling_status`.
IPC short query alias `get_scroll` reaches `get_scrolling_status`.
`jwm-tool capabilities` text lists `get_scrolling,get_scroll -> get_scrolling_status`.
`jwm-tool health` prints compact `color_management` beside occupancy.
README health text names compact `color_management`.
`tools/README.md` health text names compact `color_management`.
`jwm-tool capabilities` text lists `get_cm -> get_color_management_status`.
`jwm-tool health` prints compact `night_light` beside occupancy.
README health text names compact `night_light`.
`tools/README.md` health text names compact `night_light`.
`jwm-tool capabilities` text lists `get_nl -> get_night_light`.
`jwm-tool health` prints compact `magnifier` beside occupancy.
README health text names compact `magnifier`.
`tools/README.md` health text names compact `magnifier`.
`jwm-tool capabilities` text lists `get_mag -> get_magnifier`.
`jwm-tool health` prints compact `peek` beside occupancy.
README health text names compact `peek`.
`tools/README.md` health text names compact `peek`.
`jwm-tool capabilities` text lists `get_pk -> get_peek`.
`jwm-tool health` prints compact `expose` beside occupancy.
README health text names compact `expose`.
`tools/README.md` health text names compact `expose`.
`jwm-tool health` prints compact `gesture` beside occupancy.
README health text names compact `gesture`.
`tools/README.md` health text names compact `gesture`.
`jwm-tool capabilities` text lists `get_gest -> get_gesture`.
`jwm-tool health` prints compact `wayland` beside occupancy.
README health text names compact `wayland`.
`tools/README.md` health text names compact `wayland`.
`jwm-tool capabilities` text lists `get_wl -> get_wayland`.
`jwm-tool health` prints compact `recording` beside occupancy.
README health text names compact `recording`.
`tools/README.md` health text names compact `recording`.
`jwm-tool capabilities` text lists `get_rec -> get_recording`.
`jwm-tool health` prints compact `audio_recording` beside occupancy.
README health text names compact `audio_recording`.
`tools/README.md` health text names compact `audio_recording`.
`jwm-tool capabilities` text lists `get_arec -> get_audio_recording`.
`jwm-tool health` prints compact `capture` beside occupancy.
README health text names compact `capture`.
`tools/README.md` health text names compact `capture`.
`jwm-tool capabilities` text lists `get_cap -> get_capture`.
`jwm-tool health` prints compact `waterlily` beside occupancy.
README health text names compact `waterlily`.
`tools/README.md` health text names compact `waterlily`.
`jwm-tool capabilities` text lists `get_wly -> get_waterlily`.
`jwm-tool health` prints compact `audio` beside occupancy.
README health text names compact `audio`.
`tools/README.md` health text names compact `audio`.
`jwm-tool capabilities` text lists `get_devices -> get_audio`.
`jwm-tool health` prints compact `wallpaper` beside occupancy.
README health text names compact `wallpaper`.
`tools/README.md` health text names compact `wallpaper`.
`jwm-tool capabilities` text lists `get_wall -> get_wallpaper`.
`jwm-tool health` prints compact `bluetooth` beside occupancy.
README health text names compact `bluetooth`.
`tools/README.md` health text names compact `bluetooth`.
`jwm-tool capabilities` text lists `get_bt -> get_bluetooth`.
`jwm-tool health` prints compact `resources` beside occupancy.
README health text names compact `resources`.
`tools/README.md` health text names compact `resources`.
`jwm-tool capabilities` text lists `get_res -> get_resources`.
`jwm-tool health` prints compact `connectivity` beside occupancy.
README health text names compact `connectivity`.
`tools/README.md` health text names compact `connectivity`.
`jwm-tool capabilities` text lists `get_network -> get_connectivity`.
`jwm-tool health` prints compact `power` beside occupancy.
README health text names compact `power`.
`tools/README.md` health text names compact `power`.
`jwm-tool health` prints compact `media` beside occupancy.
README health text names compact `media`.
`tools/README.md` health text names compact `media`.
`jwm-tool capabilities` text lists `get_clip -> get_clipboard`.
`jwm-tool capabilities` text lists `get_idl -> get_idle`.
`jwm-tool capabilities` text lists `get_notif -> get_notifications`.
`jwm-tool capabilities` text lists `get_dnd -> get_do_not_disturb`.
`jwm-tool capabilities` text lists `get_pair -> get_bluetooth`.
`jwm-tool capabilities` text lists `get_conn -> get_connectivity`.
`jwm-tool capabilities` text lists `get_power -> get_power_status`.
`jwm-tool capabilities` text lists `get_media -> get_media_status`.
`docs/expose.md` names compact `expose` beside health.
`docs/tags-overview.md` names compact `tabs` beside health.
`docs/window-switcher.md` names compact `selected` beside health.
`docs/layout-picker.md` names compact `layout` beside health.
`docs/launcher.md` names compact `system_ui` beside health.
`docs/cube-effects.md` names compact `effects` beside health.
`docs/minimized-dock.md` names compact `minimized` beside health.
`docs/window-tabs.md` names compact `tabs` beside health.
`docs/clipboard.md` names compact `clipboard` beside health.
`docs/idle.md` names compact `idle` beside health.
`docs/notifications.md` names compact `notifications` beside health.
`docs/audio-recording.md` names compact `audio_recording` beside health.
`docs/wallpaper.md` names compact `wallpaper` beside health.
`docs/waterlily.md` names compact `waterlily` beside health.
`docs/hdr.md` names compact `hdr` beside health.
`docs/resources.md` names compact `resources` beside health.
`docs/media-controls.md` names compact `media` beside health.
`docs/control-center.md` names compact `system_ui` beside health.
`docs/session-menu.md` names compact `session_lock` beside health.
`docs/window-placement.md` names compact `closed_placement` beside health.
`docs/performance.md` names compact `bench` beside health.
`docs/debug-hud.md` names compact `wayland` beside health.
`docs/output-layout.md` names compact `monitors` beside health.
`docs/calendar.md` names compact `system_ui` beside health.
`docs/remote-control.md` names compact `session_lock` beside health.
`docs/startup.md` names compact `wayland` beside health.
`docs/support-bundles.md` names compact `bench` beside health.
`docs/ui-theme.md` names compact `blur` beside health.
`docs/daily-drive.md` names compact `idle` beside health.
`docs/architecture.md` names compact `tree` beside health.
README health text names `get_lock` beside compact `session_lock`.
README health text names `get_th` beside compact `tearing`.
README health text names `get_xw` beside compact `xwayland`.
README health text names `get_scroll` beside compact `scrolling`.
README health text names `get_cm` beside compact `color_management`.
README health text names `get_nl` beside compact `night_light`.
README health text names `get_mag` beside compact `magnifier`.
README health text names `get_pk` beside compact `peek`.
README health text names `get_gest` beside compact `gesture`.
README health text names `get_wl` beside compact `wayland`.
README health text names `get_rec` beside compact `recording`.
README health text names `get_arec` beside compact `audio_recording`.
README health text names `get_cap` beside compact `capture`.
README health text names `get_wly` beside compact `waterlily`.
README health text names `get_devices` beside compact `audio`.
README health text names `get_wall` beside compact `wallpaper`.
README health text names `get_bt` beside compact `bluetooth`.
README health text names `get_res` beside compact `resources`.
README health text names `get_conn` beside compact `connectivity`.
README health text names `get_clip` beside compact `clipboard`.
README health text names `get_idl` beside compact `idle`.
README health text names `get_notif` beside compact `notifications`.
README health text names `get_dnd` beside compact `dnd`.
README health text names `get_ui` beside compact `system_ui`.
README health text names `get_lt` beside compact `layout`.
README health text names `get_tab` beside compact `tabs`.
README health text names `get_sel` beside compact `selected`.
README health text names `get_strut` beside compact `struts`.
README health text names `get_pads` beside compact `scratchpads`.
README health text names `get_fw` beside compact `focused`.
README health text names `get_mons` beside compact `monitors`.
README health text names `get_ws` beside compact `workspaces`.
README health text names `get_wins` beside compact `windows`.
README health text names `get_fx` beside compact `effects`.
README health text names `get_mute` beside compact `mic`.
README health text names `get_bm` beside compact `bench`.
Wave 628: Health compact `session_lock` is the operator twin of `get_lock`.
Wave 629: Health compact `tearing` is the operator twin of `get_th`.
Wave 630: Health compact `xwayland` is the operator twin of `get_xw`.
Wave 631: Health compact `scrolling` is the operator twin of `get_scroll`.
Wave 632: Health compact `color_management` is the operator twin of `get_cm`.
Wave 633: Health compact `night_light` is the operator twin of `get_nl`.
Wave 634: Health compact `magnifier` is the operator twin of `get_mag`.
Wave 635: Health compact `peek` is the operator twin of `get_pk`.
Wave 636: Health compact `gesture` is the operator twin of `get_gest`.
Wave 637: Health compact `wayland` is the operator twin of `get_wl`.
Wave 638: Health compact `recording` is the operator twin of `get_rec`.
Wave 639: Health compact `audio_recording` is the operator twin of `get_arec`.
Wave 640: Health compact `capture` is the operator twin of `get_cap`.
Wave 641: Health compact `waterlily` is the operator twin of `get_wly`.
Wave 642: Health compact `audio` is the operator twin of `get_devices`.
Wave 643: Health compact `wallpaper` is the operator twin of `get_wall`.
Wave 644: Health compact `bluetooth` is the operator twin of `get_bt`.
Wave 645: Health compact `resources` is the operator twin of `get_res`.
Wave 646: Health compact `connectivity` is the operator twin of `get_conn`.
Wave 647: Health compact `clipboard` is the operator twin of `get_clip`.
Wave 648: Health compact `idle` is the operator twin of `get_idl`.
Wave 649: Health compact `notifications` is the operator twin of `get_notif`.
Wave 650: Health compact `dnd` is the operator twin of `get_dnd`.
Wave 651: Health compact `system_ui` is the operator twin of `get_ui`.
Wave 652: Health compact `layout` is the operator twin of `get_lt`.
Wave 653: Health compact `tabs` is the operator twin of `get_tab`.
Wave 654: Health compact `selected` is the operator twin of `get_sel`.
Wave 655: Health compact `struts` is the operator twin of `get_strut`.
Wave 656: Health compact `scratchpads` is the operator twin of `get_pads`.
Wave 657: Health compact `focused` is the operator twin of `get_fw`.
Wave 658: Health compact `monitors` is the operator twin of `get_mons`.
Wave 659: Health compact `workspaces` is the operator twin of `get_ws`.
Wave 660: Health compact `windows` is the operator twin of `get_wins`.
Wave 661: Health compact `effects` is the operator twin of `get_fx`.
Wave 662: Health compact `mic` is the operator twin of `get_mute`.
Wave 663: Health compact `bench` is the operator twin of `get_bm`.
Wave 664: Health compact `closed_placement` is the operator twin of `get_cp`.
Wave 665: Health compact `prev_layout` is the operator twin of `get_pl`.
Wave 666: Health compact `cfact` is the operator twin of `get_cf`.
Wave 667: Health compact `gaps` is the operator twin of `get_gap`.
Wave 668: Health compact `mfact` is the operator twin of `get_mf`.
Wave 669: Health compact `nmaster` is the operator twin of `get_nm`.
Wave 670: Health compact `floating` is the operator twin of `get_status.floating`.
Wave 671: Health compact `minimized` is the operator twin of `get_status.minimized`.
Wave 672: Health compact `sticky` is the operator twin of `get_status.sticky`.
Wave 673: Health compact `urgent` is the operator twin of `get_status.urgent`.
Wave 674: Health compact `fullscreen` is the operator twin of `get_status.fullscreen`.
Wave 675: Health compact `pip` is the operator twin of `get_status.pip`.
Wave 676: Health compact `blur` is the operator twin of `get_status.blur`.
Wave 677: Health compact `hdr` is the operator twin of `get_status.hdr`.
Wave 678: Health compact `expose` is the operator twin of `get_status.expose`.
Wave 679: Health compact `media` is the operator twin of `get_media`.
Wave 680: Health compact `power` is the operator twin of `get_power`.
Wave 681: `get_lock` and health compact `session_lock` share one Status nest.
Wave 682: `get_th` and health compact `tearing` share one Status nest.
Wave 683: `get_xw` and health compact `xwayland` share one Status nest.
Wave 684: `get_scroll` and health compact `scrolling` share one Status nest.
Wave 685: `get_cm` and health compact `color_management` share one Status nest.
Wave 686: `get_nl` and health compact `night_light` share one Status nest.
Wave 687: `get_mag` and health compact `magnifier` share one Status nest.
Wave 688: `get_pk` and health compact `peek` share one Status nest.
Wave 689: `get_gest` and health compact `gesture` share one Status nest.
Wave 690: `get_wl` and health compact `wayland` share one Status nest.
Wave 691: `get_rec` and health compact `recording` share one Status nest.
Wave 692: `get_arec` and health compact `audio_recording` share one Status nest.
Wave 693: `get_cap` and health compact `capture` share one Status nest.
Wave 694: `get_wly` and health compact `waterlily` share one Status nest.
Wave 695: `get_devices` and health compact `audio` share one Status nest.
Wave 696: `get_wall` and health compact `wallpaper` share one Status nest.
Wave 697: `get_bt` and health compact `bluetooth` share one Status nest.
Wave 698: `get_res` and health compact `resources` share one Status nest.
Wave 699: `get_conn` and health compact `connectivity` share one Status nest.
Wave 700: `get_clip` and health compact `clipboard` share one Status nest.
Wave 701: `get_idl` and health compact `idle` share one Status nest.
Wave 702: `get_notif` and health compact `notifications` share one Status nest.
Wave 703: `get_dnd` and health compact `dnd` share one Status nest.
Wave 704: `get_ui` and health compact `system_ui` share one Status nest.
Wave 705: `get_lt` and health compact `layout` share one Status nest.
Wave 706: `get_tab` and health compact `tabs` share one Status nest.
Wave 707: `get_sel` and health compact `selected` share one Status nest.
Wave 708: `get_strut` and health compact `struts` share one Status nest.
Wave 709: `get_pads` and health compact `scratchpads` share one Status nest.
Wave 710: `get_fw` and health compact `focused` share one Status nest.
Wave 711: `get_mons` and health compact `monitors` share one Status nest.
Wave 712: `get_ws` and health compact `workspaces` share one Status nest.
Wave 713: `get_wins` and health compact `windows` share one Status nest.
Wave 714: `get_fx` and health compact `effects` share one Status nest.
Wave 715: `get_mute` and health compact `mic` share one Status nest.
Wave 716: `get_bm` and health compact `bench` share one Status nest.
Wave 717: `get_cp` and health compact `closed_placement` share one Status nest.
Wave 718: `get_pl` and health compact `prev_layout` share one Status nest.
Wave 719: `get_cf` and health compact `cfact` share one Status nest.
Wave 720: `get_gap` and health compact `gaps` share one Status nest.
Wave 721: `get_mf` and health compact `mfact` share one Status nest.
Wave 722: `get_nm` and health compact `nmaster` share one Status nest.
Wave 723: `get_status.floating` and health compact `floating` share one Status nest.
Wave 724: `get_status.minimized` and health compact `minimized` share one Status nest.
Wave 725: `get_status.sticky` and health compact `sticky` share one Status nest.
Wave 726: `get_status.urgent` and health compact `urgent` share one Status nest.
Wave 727: `get_status.fullscreen` and health compact `fullscreen` share one Status nest.
Wave 728: `get_status.pip` and health compact `pip` share one Status nest.
Wave 729: `get_status.blur` and health compact `blur` share one Status nest.
Wave 730: `get_status.hdr` and health compact `hdr` share one Status nest.
Wave 731: `get_status.expose` and health compact `expose` share one Status nest.
Wave 732: `get_media` and health compact `media` share one Status nest.
Wave 733: `get_power` and health compact `power` share one Status nest.
Wave 734: Doctor bundles include health compact `session_lock` from `get_lock`.
Wave 735: Doctor bundles include health compact `tearing` from `get_th`.
Wave 736: Doctor bundles include health compact `xwayland` from `get_xw`.
Wave 737: Doctor bundles include health compact `scrolling` from `get_scroll`.
Wave 738: Doctor bundles include health compact `color_management` from `get_cm`.
Wave 739: Doctor bundles include health compact `night_light` from `get_nl`.
Wave 740: Doctor bundles include health compact `magnifier` from `get_mag`.
Wave 741: Doctor bundles include health compact `peek` from `get_pk`.
Wave 742: Doctor bundles include health compact `gesture` from `get_gest`.
Wave 743: Doctor bundles include health compact `wayland` from `get_wl`.
Wave 744: Doctor bundles include health compact `recording` from `get_rec`.
Wave 745: Doctor bundles include health compact `audio_recording` from `get_arec`.
Wave 746: Doctor bundles include health compact `capture` from `get_cap`.
Wave 747: Doctor bundles include health compact `waterlily` from `get_wly`.
Wave 748: Doctor bundles include health compact `audio` from `get_devices`.
Wave 749: Doctor bundles include health compact `wallpaper` from `get_wall`.
Wave 750: Doctor bundles include health compact `bluetooth` from `get_bt`.
Wave 751: Doctor bundles include health compact `resources` from `get_res`.
Wave 752: Doctor bundles include health compact `connectivity` from `get_conn`.
Wave 753: Doctor bundles include health compact `clipboard` from `get_clip`.
Wave 754: Doctor bundles include health compact `idle` from `get_idl`.
Wave 755: Doctor bundles include health compact `notifications` from `get_notif`.
Wave 756: Doctor bundles include health compact `dnd` from `get_dnd`.
Wave 757: Doctor bundles include health compact `system_ui` from `get_ui`.
Wave 758: Doctor bundles include health compact `layout` from `get_lt`.
Wave 759: Doctor bundles include health compact `tabs` from `get_tab`.
Wave 760: Doctor bundles include health compact `selected` from `get_sel`.
Wave 761: Doctor bundles include health compact `struts` from `get_strut`.
Wave 762: Doctor bundles include health compact `scratchpads` from `get_pads`.
Wave 763: Doctor bundles include health compact `focused` from `get_fw`.
Wave 764: Doctor bundles include health compact `monitors` from `get_mons`.
Wave 765: Doctor bundles include health compact `workspaces` from `get_ws`.
Wave 766: Doctor bundles include health compact `windows` from `get_wins`.
Wave 767: Doctor bundles include health compact `effects` from `get_fx`.
Wave 768: Doctor bundles include health compact `mic` from `get_mute`.
Wave 769: Doctor bundles include health compact `bench` from `get_bm`.
Wave 770: Doctor bundles include health compact `closed_placement` from `get_cp`.
Wave 771: Doctor bundles include health compact `prev_layout` from `get_pl`.
Wave 772: Doctor bundles include health compact `cfact` from `get_cf`.
Wave 773: Doctor bundles include health compact `gaps` from `get_gap`.
Wave 774: Doctor bundles include health compact `mfact` from `get_mf`.
Wave 775: Doctor bundles include health compact `nmaster` from `get_nm`.
Wave 776: Doctor bundles include health compact `floating` from `get_status.floating`.
Wave 777: Doctor bundles include health compact `minimized` from `get_status.minimized`.
Wave 778: Doctor bundles include health compact `sticky` from `get_status.sticky`.
Wave 779: Doctor bundles include health compact `urgent` from `get_status.urgent`.
Wave 780: Doctor bundles include health compact `fullscreen` from `get_status.fullscreen`.
Wave 781: Doctor bundles include health compact `pip` from `get_status.pip`.
Wave 782: Doctor bundles include health compact `blur` from `get_status.blur`.
Wave 783: Doctor bundles include health compact `hdr` from `get_status.hdr`.
Wave 784: Doctor bundles include health compact `expose` from `get_status.expose`.
Wave 785: Doctor bundles include health compact `media` from `get_media`.
Wave 786: Doctor bundles include health compact `power` from `get_power`.
Wave 787: Support triage reads health compact `session_lock` before `get_lock` dumps.
Wave 788: Support triage reads health compact `tearing` before `get_th` dumps.
Wave 789: Support triage reads health compact `xwayland` before `get_xw` dumps.
Wave 790: Support triage reads health compact `scrolling` before `get_scroll` dumps.
Wave 791: Support triage reads health compact `color_management` before `get_cm` dumps.
Wave 792: Support triage reads health compact `night_light` before `get_nl` dumps.
Wave 793: Support triage reads health compact `magnifier` before `get_mag` dumps.
Wave 794: Support triage reads health compact `peek` before `get_pk` dumps.
Wave 795: Support triage reads health compact `gesture` before `get_gest` dumps.
Wave 796: Support triage reads health compact `wayland` before `get_wl` dumps.
Wave 797: Support triage reads health compact `recording` before `get_rec` dumps.
Wave 798: Support triage reads health compact `audio_recording` before `get_arec` dumps.
Wave 799: Support triage reads health compact `capture` before `get_cap` dumps.
Wave 800: Support triage reads health compact `waterlily` before `get_wly` dumps.
Wave 801: Support triage reads health compact `audio` before `get_devices` dumps.
Wave 802: Support triage reads health compact `wallpaper` before `get_wall` dumps.
Wave 803: Support triage reads health compact `bluetooth` before `get_bt` dumps.
Wave 804: Support triage reads health compact `resources` before `get_res` dumps.
Wave 805: Support triage reads health compact `connectivity` before `get_conn` dumps.
Wave 806: Support triage reads health compact `clipboard` before `get_clip` dumps.
Wave 807: Support triage reads health compact `idle` before `get_idl` dumps.
Wave 808: Support triage reads health compact `notifications` before `get_notif` dumps.
Wave 809: Support triage reads health compact `dnd` before `get_dnd` dumps.
Wave 810: Support triage reads health compact `system_ui` before `get_ui` dumps.
Wave 811: Support triage reads health compact `layout` before `get_lt` dumps.
Wave 812: Support triage reads health compact `tabs` before `get_tab` dumps.
Wave 813: Support triage reads health compact `selected` before `get_sel` dumps.
Wave 814: Support triage reads health compact `struts` before `get_strut` dumps.
Wave 815: Support triage reads health compact `scratchpads` before `get_pads` dumps.
Wave 816: Support triage reads health compact `focused` before `get_fw` dumps.
Wave 817: Support triage reads health compact `monitors` before `get_mons` dumps.
Wave 818: Support triage reads health compact `workspaces` before `get_ws` dumps.
Wave 819: Support triage reads health compact `windows` before `get_wins` dumps.
Wave 820: Support triage reads health compact `effects` before `get_fx` dumps.
Wave 821: Support triage reads health compact `mic` before `get_mute` dumps.
Wave 822: Support triage reads health compact `bench` before `get_bm` dumps.
Wave 823: Support triage reads health compact `closed_placement` before `get_cp` dumps.
Wave 824: Support triage reads health compact `prev_layout` before `get_pl` dumps.
Wave 825: Support triage reads health compact `cfact` before `get_cf` dumps.
Wave 826: Support triage reads health compact `gaps` before `get_gap` dumps.
Wave 827: Support triage reads health compact `mfact` before `get_mf` dumps.
Wave 828: Support triage reads health compact `nmaster` before `get_nm` dumps.
Wave 829: Support triage reads health compact `floating` before `get_status.floating` dumps.
Wave 830: Support triage reads health compact `minimized` before `get_status.minimized` dumps.
Wave 831: Support triage reads health compact `sticky` before `get_status.sticky` dumps.
Wave 832: Support triage reads health compact `urgent` before `get_status.urgent` dumps.
Wave 833: Support triage reads health compact `fullscreen` before `get_status.fullscreen` dumps.
Wave 834: Support triage reads health compact `pip` before `get_status.pip` dumps.
Wave 835: Support triage reads health compact `blur` before `get_status.blur` dumps.
Wave 836: Support triage reads health compact `hdr` before `get_status.hdr` dumps.
Wave 837: Support triage reads health compact `expose` before `get_status.expose` dumps.
Wave 838: Support triage reads health compact `media` before `get_media` dumps.
Wave 839: Support triage reads health compact `power` before `get_power` dumps.
Wave 840: Nested smoke checks health compact `session_lock` after `get_lock`.
Wave 841: Nested smoke checks health compact `tearing` after `get_th`.
Wave 842: Nested smoke checks health compact `xwayland` after `get_xw`.
Wave 843: Nested smoke checks health compact `scrolling` after `get_scroll`.
Wave 844: Nested smoke checks health compact `color_management` after `get_cm`.
Wave 845: Nested smoke checks health compact `night_light` after `get_nl`.
Wave 846: Nested smoke checks health compact `magnifier` after `get_mag`.
Wave 847: Nested smoke checks health compact `peek` after `get_pk`.
Wave 848: Nested smoke checks health compact `gesture` after `get_gest`.
Wave 849: Nested smoke checks health compact `wayland` after `get_wl`.
Wave 850: Nested smoke checks health compact `recording` after `get_rec`.
Wave 851: Nested smoke checks health compact `audio_recording` after `get_arec`.
Wave 852: Nested smoke checks health compact `capture` after `get_cap`.
Wave 853: Nested smoke checks health compact `waterlily` after `get_wly`.
Wave 854: Nested smoke checks health compact `audio` after `get_devices`.
Wave 855: Nested smoke checks health compact `wallpaper` after `get_wall`.
Wave 856: Nested smoke checks health compact `bluetooth` after `get_bt`.
Wave 857: Nested smoke checks health compact `resources` after `get_res`.
Wave 858: Nested smoke checks health compact `connectivity` after `get_conn`.
Wave 859: Nested smoke checks health compact `clipboard` after `get_clip`.
Wave 860: Nested smoke checks health compact `idle` after `get_idl`.
Wave 861: Nested smoke checks health compact `notifications` after `get_notif`.
Wave 862: Nested smoke checks health compact `dnd` after `get_dnd`.
Wave 863: Nested smoke checks health compact `system_ui` after `get_ui`.
Wave 864: Nested smoke checks health compact `layout` after `get_lt`.
Wave 865: Nested smoke checks health compact `tabs` after `get_tab`.
Wave 866: Nested smoke checks health compact `selected` after `get_sel`.
Wave 867: Nested smoke checks health compact `struts` after `get_strut`.
Wave 868: Nested smoke checks health compact `scratchpads` after `get_pads`.
Wave 869: Nested smoke checks health compact `focused` after `get_fw`.
Wave 870: Nested smoke checks health compact `monitors` after `get_mons`.
Wave 871: Nested smoke checks health compact `workspaces` after `get_ws`.
Wave 872: Nested smoke checks health compact `windows` after `get_wins`.
Wave 873: Nested smoke checks health compact `effects` after `get_fx`.
Wave 874: Nested smoke checks health compact `mic` after `get_mute`.
Wave 875: Nested smoke checks health compact `bench` after `get_bm`.
Wave 876: Nested smoke checks health compact `closed_placement` after `get_cp`.
Wave 877: Nested smoke checks health compact `prev_layout` after `get_pl`.
Wave 878: Nested smoke checks health compact `cfact` after `get_cf`.
Wave 879: Nested smoke checks health compact `gaps` after `get_gap`.
Wave 880: Nested smoke checks health compact `mfact` after `get_mf`.
Wave 881: Nested smoke checks health compact `nmaster` after `get_nm`.
Wave 882: Nested smoke checks health compact `floating` after `get_status.floating`.
Wave 883: Nested smoke checks health compact `minimized` after `get_status.minimized`.
Wave 884: Nested smoke checks health compact `sticky` after `get_status.sticky`.
Wave 885: Nested smoke checks health compact `urgent` after `get_status.urgent`.
Wave 886: Nested smoke checks health compact `fullscreen` after `get_status.fullscreen`.
Wave 887: Nested smoke checks health compact `pip` after `get_status.pip`.
Wave 888: Nested smoke checks health compact `blur` after `get_status.blur`.
Wave 889: Nested smoke checks health compact `hdr` after `get_status.hdr`.
Wave 890: Nested smoke checks health compact `expose` after `get_status.expose`.
Wave 891: Nested smoke checks health compact `media` after `get_media`.
Wave 892: Nested smoke checks health compact `power` after `get_power`.
Wave 893: Upgrade notes keep health compact `session_lock` beside `get_lock`.
Wave 894: Upgrade notes keep health compact `tearing` beside `get_th`.
Wave 895: Upgrade notes keep health compact `xwayland` beside `get_xw`.
Wave 896: Upgrade notes keep health compact `scrolling` beside `get_scroll`.
Wave 897: Upgrade notes keep health compact `color_management` beside `get_cm`.
Wave 898: Upgrade notes keep health compact `night_light` beside `get_nl`.
Wave 899: Upgrade notes keep health compact `magnifier` beside `get_mag`.
Wave 900: Upgrade notes keep health compact `peek` beside `get_pk`.
Wave 901: Upgrade notes keep health compact `gesture` beside `get_gest`.
Wave 902: Upgrade notes keep health compact `wayland` beside `get_wl`.
Wave 903: Upgrade notes keep health compact `recording` beside `get_rec`.
Wave 904: Upgrade notes keep health compact `audio_recording` beside `get_arec`.
Wave 905: Upgrade notes keep health compact `capture` beside `get_cap`.
Wave 906: Upgrade notes keep health compact `waterlily` beside `get_wly`.
Wave 907: Upgrade notes keep health compact `audio` beside `get_devices`.
Wave 908: Upgrade notes keep health compact `wallpaper` beside `get_wall`.
Wave 909: Upgrade notes keep health compact `bluetooth` beside `get_bt`.
Wave 910: Upgrade notes keep health compact `resources` beside `get_res`.
Wave 911: Upgrade notes keep health compact `connectivity` beside `get_conn`.
Wave 912: Upgrade notes keep health compact `clipboard` beside `get_clip`.
Wave 913: Upgrade notes keep health compact `idle` beside `get_idl`.
Wave 914: Upgrade notes keep health compact `notifications` beside `get_notif`.
Wave 915: Upgrade notes keep health compact `dnd` beside `get_dnd`.
Wave 916: Upgrade notes keep health compact `system_ui` beside `get_ui`.
Wave 917: Upgrade notes keep health compact `layout` beside `get_lt`.
Wave 918: Upgrade notes keep health compact `tabs` beside `get_tab`.
Wave 919: Upgrade notes keep health compact `selected` beside `get_sel`.
Wave 920: Upgrade notes keep health compact `struts` beside `get_strut`.
Wave 921: Upgrade notes keep health compact `scratchpads` beside `get_pads`.
Wave 922: Upgrade notes keep health compact `focused` beside `get_fw`.
Wave 923: Upgrade notes keep health compact `monitors` beside `get_mons`.
Wave 924: Upgrade notes keep health compact `workspaces` beside `get_ws`.
Wave 925: Upgrade notes keep health compact `windows` beside `get_wins`.
Wave 926: Upgrade notes keep health compact `effects` beside `get_fx`.
Wave 927: Upgrade notes keep health compact `mic` beside `get_mute`.
Wave 928: Upgrade notes keep health compact `bench` beside `get_bm`.
Wave 929: Upgrade notes keep health compact `closed_placement` beside `get_cp`.
Wave 930: Upgrade notes keep health compact `prev_layout` beside `get_pl`.
Wave 931: Upgrade notes keep health compact `cfact` beside `get_cf`.
Wave 932: Upgrade notes keep health compact `gaps` beside `get_gap`.
Wave 933: Upgrade notes keep health compact `mfact` beside `get_mf`.
Wave 934: Upgrade notes keep health compact `nmaster` beside `get_nm`.
Wave 935: Upgrade notes keep health compact `floating` beside `get_status.floating`.
Wave 936: Upgrade notes keep health compact `minimized` beside `get_status.minimized`.
Wave 937: Upgrade notes keep health compact `sticky` beside `get_status.sticky`.
Wave 938: Upgrade notes keep health compact `urgent` beside `get_status.urgent`.
Wave 939: Upgrade notes keep health compact `fullscreen` beside `get_status.fullscreen`.
Wave 940: Upgrade notes keep health compact `pip` beside `get_status.pip`.
Wave 941: Upgrade notes keep health compact `blur` beside `get_status.blur`.
Wave 942: Upgrade notes keep health compact `hdr` beside `get_status.hdr`.
Wave 943: Upgrade notes keep health compact `expose` beside `get_status.expose`.
Wave 944: Upgrade notes keep health compact `media` beside `get_media`.
Wave 945: Upgrade notes keep health compact `power` beside `get_power`.
Wave 946: Compatibility tables name `get_lock` with health compact `session_lock`.
Wave 947: Compatibility tables name `get_th` with health compact `tearing`.
Wave 948: Compatibility tables name `get_xw` with health compact `xwayland`.
Wave 949: Compatibility tables name `get_scroll` with health compact `scrolling`.
Wave 950: Compatibility tables name `get_cm` with health compact `color_management`.
Wave 951: Compatibility tables name `get_nl` with health compact `night_light`.
Wave 952: Compatibility tables name `get_mag` with health compact `magnifier`.
