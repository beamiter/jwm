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
row leads with the window's application icon, resolved from its class (via
`StartupWMClass`) through the same cached resolver the launcher uses; a
window whose class resolves to nothing keeps the generic window glyph —
there is never an empty hole. Then
the title, the class when it adds information, and a `screen N`
marker on the other heads.

A window earns a row only when the gesture could actually land on it:

- not [swallowed](../README.md) by its terminal;
- sticky, or on one of its monitor's active tags — a scratchpad parked on
  no tag drops out.

Minimized windows keep their rows: they interleave with the visible ones
in MRU order, carry a `[minimised]` marker, and picking one restores it —
through the same transition the launcher and the Dock use — before it is
focused. When the window behind the current one in MRU order is minimized,
a single tap of `Alt+Tab` therefore restores it; that is intended.

The list is a snapshot taken when the switcher opens: a window created
mid-gesture gets no row, and one that dies or loses its tag while you hold
the modifier fails the commit-time re-check and degrades the gesture to a
cancel rather than focusing nothing. A restore that fails mid-gesture
degrades the same way. The one mid-gesture edit is yours: `Delete` removes
a row as its window closes, and the survivors keep their MRU order.

## Keys while the panel is up

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab`, `Up` / `Down` | move the highlight, wrapping around both ends |
| `Return` | commit the highlighted window |
| `Delete` / `BackSpace` | close the highlighted window without leaving the gesture — the next-oldest window slides under the highlight; closing the last row ends the gesture |
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

The switcher takes the same button grab every other clickable panel takes,
so every press reaches the panel rather than the window its rows are drawn
over — which matters because the row a click lands on is drawn over exactly
that window. A left click on a row commits it; a left click anywhere else,
and any other button anywhere, cancels.

The wheel is the exception, and it browses rather than cancels: a scroll
over the panel steps the highlight the way it does on every other panel,
because a touchpad flick while the modifier is still held is asking for the
next row, not for the switch in flight to be thrown away. A scroll on the
dimmed area outside the panel does nothing, and neither does a horizontal
one anywhere — there is nothing sideways to browse.

If another client already holds the pointer — a drag in flight, a menu's own
grab — the gesture is not spent on it: `Alt+Tab` is a keyboard gesture first,
so the panel opens keyboard-only and a click then behaves as it did before
the grab existed (on X11 the server may deliver it to the window under the
panel).

## Where it lives

`src/jwm/features/switcher.rs` carries the gesture's pure logic —
eligibility, the initial selection, row text, commit validation — unit
tested without a display. The panel itself is an ordinary system-UI list,
reusing the launcher's row format, so no switcher-specific rendering code
exists.
