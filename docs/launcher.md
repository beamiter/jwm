# Application launcher

`Alt+R` (`app_launcher`) opens the launcher. Type to filter, `Up`/`Down` (or
`Tab`) to move, `Enter` to launch, `Esc` to close.

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
