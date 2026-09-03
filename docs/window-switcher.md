# Window switcher (Alt+Tab)

`Alt+Tab` is a hold-the-modifier, most-recently-used window switcher. Hold
`Alt`, tap `Tab` to walk the list, let go of `Alt` to switch to the
highlighted window. One tap is the classic "go back": the highlight opens
on the *previous* window, and a quick tap that is already over by the time
the panel is up commits it immediately.

> **Default keybinding change.** `Alt+Tab` / `Alt+Shift+Tab` used to run
> `loopview` (workspace cycling). They now run `window_switcher(±1)`, and
> workspace cycling moved to `Alt+Page_Up` / `Alt+Page_Down`. Touchpad
> swipe bindings are unaffected — `loopview` remains an ordinary bindable
> command. To put the old arrangement back, edit the two `Tab` entries in
> the `[[keys]]` tables of your [configuration](startup.md):
>
> ```toml
> [[keys]]
> modifier = ["Mod1"]
> key = "Tab"
> function = "loopview"
> argument = { Int = 1 }
>
> [[keys]]
> modifier = ["Mod1", "Shift"]
> key = "Tab"
> function = "loopview"
> argument = { Int = -1 }
> ```

## The list

Rows are the most-recently-used windows, the monitor in front of you
first — the same order the [launcher's window list](launcher.md) uses. A
row shows the title, the class when it adds information, and a `screen N`
marker on the other heads.

A window earns a row only when the gesture could actually land on it:

- not minimized — switching to a minimized window would have to restore it
  first, and the gesture deliberately does not restore;
- not [swallowed](../README.md) by its terminal, not hidden;
- sticky, or on one of its monitor's active tags — a scratchpad parked on
  no tag drops out.

The list is a snapshot taken when the switcher opens: a window created
mid-gesture gets no row, and one that dies or loses its tag while you hold
the modifier fails the commit-time re-check and degrades the gesture to a
cancel rather than focusing nothing.

## Keys while the panel is up

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab`, `Up` / `Down` | move the highlight, wrapping around both ends |
| `Return` | commit the highlighted window |
| `Esc` | cancel |
| release `Alt` (or `Super`/`Ctrl`) | commit the highlighted window |
| release `Shift` | nothing — letting Shift go first in `Alt+Shift+Tab` must not end the gesture early |

Wrapping is deliberate here, and opposite to [expose's](expose.md) clamp:
the gesture is a loop through recent windows, not a position on a grid.

The keyboard is grabbed and *every* key is consumed while the switcher is
up, so nothing leaks to the window underneath. Re-triggering the binding
(or calling `window_switcher` over IPC) while it is open steps the list
instead of rebuilding it, and the switcher never stacks on top of another
modal panel.

## Pointer

The pointer stays free: clicking a row commits that window, clicking
anywhere else cancels.

## Where it lives

`src/jwm/features/switcher.rs` carries the gesture's pure logic —
eligibility, the initial selection, row text, commit validation — unit
tested without a display. The panel itself is an ordinary system-UI list,
reusing the launcher's row format, so no switcher-specific rendering code
exists.
