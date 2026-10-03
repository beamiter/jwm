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
`get_nmaster`, `get_show_bar`, and `get_prev_layout` return the selected
monitor's live values (optional `connector` when known). `get_show_bar`
also reports `bar_visible`, `has_visible_fullscreen` and
`owns_output_count` (`get_bar` /
`get_bar_visible` / `get_owns_output` / `get_visible_fullscreen` /
`get_vf` alias the same snapshot). A
per-monitor show-bar snapshot is available for any output, not only the
focused one (JSON keys `monitor` / `tag` / `show_bar` / `bar_visible` /
`has_visible_fullscreen` / `owns_output_count` / optional `connector`).
They also include `layout` (`lt_symbol`). They also include `gap`.
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
(including `get_visible_fullscreen` / `get_vf`).
`jwm-tool msg` help lists `get_show_bar`, `get_vf`, `get_owns_output`
and `get_visible_fullscreen` / `get_bar_visible` / `get_bar`.
`jwm-tool msg` after-help examples include `get_vf` and `get_show_bar`.
`jwm-tool capabilities` text lists `bar->monitor/bar` and occupancy
query aliases of `get_show_bar`.
`jwm-tool health` prints focused-bar occupancy when nested, including
`has_visible_fullscreen`. The line appends `connector` when known and includes the monitor
number and the current `tag` and `layout` and `gap`.
README control examples include `get_show_bar` and `get_vf`, as does
`tools/README.md`.
`toggletag` on a fullscreen client emits `monitor/bar`, as does `tag`
and sticky.
`get_workspaces` / `get_tree` also report pip / maximized / above / below /
fixed (and tree scratchpad / tabbed) counts; monitors report
`window_count` / floating / minimized / sticky counts; windows report
optional `stack_index`.