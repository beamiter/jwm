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
`locked` flag per monitor, so a status bar can show which screens are down.
