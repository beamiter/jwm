# Application launcher

`Alt+R` (`app_launcher`) opens the launcher. Type to filter, `Up`/`Down` (or
`Tab`) to move, `Enter` to launch, `Esc` — or `Alt+R` again — to close.

Applications come from the desktop entries in `$XDG_DATA_HOME/applications` and
`$XDG_DATA_DIRS`, plus every executable on `PATH` that no desktop entry already
named. Entries marked `Hidden=true` or `NoDisplay=true` are left out, because
their author asked for that.

## The list is ordered by what you actually run

Launches are remembered, and the ranking is *frecency*: how often, weighted by
how recently.

- **What you typed decides first.** History only breaks ties. Typing `firef`
  must not open the file manager because you open it more often — a launcher
  that stops obeying the query is worse than one with no ranking at all.
- **With an empty query everything ties**, so the whole list is ordered by use.
  This is the case that matters most: the top row is one `Enter` away.
- Recency is bucketed — this hour, today, this week, this month, older — rather
  than decayed continuously. When a row moves, the reason should be something
  you can state, and "I used it today" is easier to reason about than a
  half-life.
- Launch counts stop counting past 50. Otherwise an editor opened ten thousand
  times would sit at the top for months after you stopped using it.

The history lives in `$XDG_DATA_HOME/jwm/launcher-usage` (usually
`~/.local/share/jwm/launcher-usage`), one `count last_used name` per line. It is
written on each launch rather than on exit, so an abrupt end to the session does
not lose it, and it is capped at 500 entries — the least useful go first.
Deleting the file resets the ranking; a corrupt line costs that line and nothing
else.

## Arithmetic

A query containing an operator is treated as a question rather than a search:

```
1920*0.6   →   =  1152
```

`Enter` copies the result to the clipboard (and to the [clipboard
history](clipboard.md)). `+ - * / % ^` work, with parentheses, unary minus, and
the usual precedence; `^` associates to the right, so `2^3^2` is 512.

An operator is **required** — a query of `42` is somebody looking for an
application, not asking what 42 is. Queries that merely contain an operator
character but do not parse (`gtk+`, `c++`, `re-search`) stay searches, and
division by zero shows nothing rather than `inf`.

While an answer is showing it replaces the application list, so `Enter` has
exactly one meaning. Backspacing past the operator brings the list back.

## Open windows

Once you type something, the list also matches the windows that are already
open, marked with a  glyph:

```
  Beta window — xmessage
  Alpha window — xmessage  [tag 1]
 xmessage
```

`Enter` on one of those **focuses** it rather than starting a second copy:
JWM moves to its monitor, switches to its tag, un-minimises it if it was
minimised, and raises it. Nothing is spawned and the launch ranking is not
touched — focusing Firefox must not promote the Firefox application row.

`/` on its own lists open windows and no applications at all, which is the
window switcher when you want one:

```
/            every window
/git         windows matching "git" by title, class or instance
```

The prefix cannot collide with the calculator, because no expression starts
with an operator: `/1+1` filters windows and does not compute 2.

### How the two kinds are ordered

1. **What you typed decides**, across both kinds. A better-matching
   application still beats a worse-matching window.
2. **On an equal score the window comes first.** Focusing something that
   already exists is one keystroke to undo; a second copy of a browser or an
   IDE may not be.
3. **Windows keep most-recently-focused order**, and the monitor you are
   looking at comes before the other one.
4. **Applications keep their frecency**, exactly as before.

With an **empty query the list is applications only**. The promise that the
top row is one `Enter` from the application you use most is worth more than a
window list you did not ask for; one keystroke brings the windows in.

A row says where its window is only when that is somewhere else — `[tag 3]`,
`[minimised]`, `[screen 1]` for one that is on the other monitor and plainly
visible there — so it answers "will `Enter` move me somewhere" before you
press it. A window matches on its title, its class **or** its instance, but
never across two of them: `foxgithub` finds nothing, because a match spanning
`GitHub` and `firefox` is a match in neither.

Two kinds of window are deliberately absent: one hidden by the scratchpad
(it sits on no tag at all, and revealing it means duplicating the
scratchpad's own logic) and a terminal that has been
[swallowed](../README.md) by its child, which is standing in for that child
rather than being a window of its own.

## Terminal applications

A desktop entry with `Terminal=true` — an editor, a system monitor, a package
manager front end — draws no window of its own. Launched directly it would exit
instantly and look like a launcher that did nothing, so JWM hands it a terminal:

```
<your terminal> -e htop
```

Those rows are marked with a `` glyph in the list. The terminal is the one
`JWM_TERMINAL` names, or the first one JWM's terminal prober finds. An
executable found on `PATH` rather than in a desktop entry declares nothing about
this, so it is launched as-is rather than guessed at.
