# Calendar

`Alt+F9` (`calendar`) opens a month card: the full date and time on top, then
the month laid out as a grid with today in brackets.

| Key | Action |
| --- | --- |
| `Left` / `Right` | previous / next month |
| `Up` / `Down` | previous / next year |
| `t` / `Home` | back to the month containing today |
| `Esc` or `Alt+F9` | close — the key that opened the card also dismisses it |
| another panel key | hand the screen to that panel, closing the card |

The grid's blank edge cells answer the pointer: a click on a leading cell
before the 1st — where the previous month's tail days would sit — flips to
the previous month, a click on a trailing cell past the month's end flips to
the next, and a click on today's bracketed cell returns to the current
month, the pointer counterparts of `Left`/`Right` and `t`. Clicks on
ordinary days, the clock line and the header remain no-ops. The footer hint
advertises the gesture:
`←/→  month    ↑/↓  year    t  today    click edge days  month    Esc  close`.

Weeks start on Monday (the ISO week). Today is only bracketed in its own
month, so paging away makes it clear you are looking somewhere else.

The clock line is captured when the card opens and does not tick — the card is
a glance, not a widget, and a repainting clock would keep the compositor busy
for no reason. Close and reopen for the current time.

Everything about the layout is pure and takes the date as an argument, so
month lengths, leap years (including the 1900/2000 century rule), the weekday
the first falls on, the year rollover at December, and the mapping from a
clicked grid cell to its month flip are unit tested against fixed dates
rather than whatever "today" happens to be.
