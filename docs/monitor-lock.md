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