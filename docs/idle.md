# Idle

When nobody has touched the machine for a while, JWM dims the screen, and — if
you ask it to — locks it and powers the displays down. Any input undoes the
dim; only the password dismisses the lock.

While locked, `Backspace` removes one character and `Esc` securely clears the
whole entered password plus any previous authentication error; neither key
unlocks or closes the surface.

The locked card leads with the current time (`HH:MM`, 24-hour like the
calendar card) and the spelled-out date above the password row; the clock
repaints on each wall-clock minute through a wakeup that is scheduled only
while the lock is up. A quiet "Caps Lock is on" row appears under the
password row while caps lock is active — informational, not an error, and it
coexists with the wrong-password message. The indicator is honest about the
field too: caps lock genuinely shifts the ASCII letters typed into the
password (shift with caps types lowercase again), while digits and
punctuation follow the shift key alone, exactly as an unlocked prompt
behaves.

While a media player is active the card trails a now-playing row:
`Title — Artist`, the `m:ss / m:ss` position when the player reports one,
and the trailing status icon — the control-center media row minus its
transport cluster, so a paused player reads exactly as paused as it does
there. The lock reveals only what the session's own control center already
shows: no album art, and no controls (the transport keys already work
while locked; they need no on-screen cluster). With no player active the
row is absent rather than blank, so a player-less lock is byte-identical
to one built before the row existed. The row rides the bridge's
three-second push — the overlay re-syncs only when the visible text
actually changes, so a paused player's identical re-polls cost one
comparison — and it is seeded when the lock opens, so it is there from
the first frame.

Volume, brightness and media transport keys stay live behind the lock, as
they do on GNOME, KDE, macOS and Windows — exactly the ten dedicated XF86
keysyms (volume raise/lower/mute, play/pause/next/previous/stop, brightness
up/down), and only where the key is bound to the matching media action, so
it runs the same dispatch it would unlocked, controls worker included.
Every other key is still swallowed, and the feedback is deliberately
invisible: the opaque backdrop hides the OSD, and the card grows no rows
for it.

`Enter` no longer runs PAM on the compositor. A wrong password costs ~2 s
inside pam_unix, and pam_sss, fingerprint or faillock modules can wait
arbitrarily long, so the attempt runs on a one-shot worker thread while the
compositor keeps rendering, and the status row reads "Verifying…" through
the same channel a failure uses. While one attempt is in flight `Enter` is
dead, typing collects the next attempt (and clears the row, exactly as it
clears an error), and `Esc` still only clears the field — it cannot cancel
the worker. The password is wiped on every path out of the attempt, and
only a success unlocks, through the same code path as before.

```toml
[behavior]
idle_dim_secs = 120           # 0 switches the stage off
idle_dim_level = 0.35         # fraction of normal brightness while dimmed
idle_lock_secs = 0            # off by default, see below
idle_screen_off_secs = 0
idle_screen_off_command = ""  # e.g. "xset dpms force off"
idle_screen_on_command = ""   # e.g. "xset dpms force on"
```

Each stage is judged against its own timeout, so a configuration whose stages
are out of order still behaves sensibly — the earlier one simply happens first.
Setting every stage to 0 switches the whole policy off, and JWM then never
reads the idle clock at all.

`jwm --check-config` reads these keys before a session ever starts: it warns
when `idle_lock_secs` is below the floor described below, and when
`idle_dim_level` falls outside `[0, 1]` — a level the dim stage replaces with
`0.35`, saying so once in the log of the session that never ran the check.

## Why locking is off by default

The lock screen authenticates against PAM. On a machine where PAM cannot be
reached, the password is rejected and the session is locked out of itself —
which is a fine risk to accept deliberately and a poor one to inherit from a
default. Turning it on is a decision only you can make:

```toml
idle_lock_secs = 600
```

Test it once with `jwm-tool msg lock_screen` before trusting a timeout to do
it while you are away from the keyboard.

### The two guards on the lock stage

A lock timeout is the one stage that can take the session away from the person
using it, so two rules keep a misconfiguration from locking you out of your own
desktop:

- **A floor of 60 seconds.** A non-zero `idle_lock_secs` below 60 is raised to
  60, with one warning in the log. `idle_lock_secs = 1` otherwise re-locks
  between the keystrokes of the password, and the only way back in is editing
  the config from behind the lock screen. `0` still switches the stage off
  outright.
- **A minute of grace after every unlock.** Typing the password is a statement
  that somebody is at the keyboard, so the lock stage does not re-arm for 60
  seconds afterwards. Dimming and the screen-off stage are unaffected.

If a lock cannot be shown, what happens next depends on why, because an
unattended session that stops asking stays unlocked until somebody touches
the keyboard:

- **Something that passes on its own** — another panel open, something else
  holding the pointer grab — is retried every 5 seconds for as long as the
  session stays idle.
- **A refusal the backend could not explain** — it tried to start the
  compositor the lock card draws on and the attempt returned an error, which
  covers a VT switch, DRM master briefly held elsewhere and a momentary
  renderer failure alike — is retried on the same 5-second interval, but only
  12 times. That is about a minute of asking: long enough to outlast the
  transient causes, bounded so a machine where it will never work is not
  asked all night.
- **A backend that reports it cannot start a compositor at all** is not asked
  again inside this idle period. It is a statement about the backend rather
  than about this moment; the next idle period asks once more.

The first failure is a warning in the log and the repeats are at debug level;
giving up is one final warning that says which of the two reasons it was.

## Powering the displays down

JWM does not do this itself. Which knob is right depends on the session — `xset
dpms force off` under X11, `wlopm --off '*'` under a Wayland session, a
vendor tool on some laptops — and choosing wrong leaves a screen that is black
for the wrong reason. Name the command instead:

```toml
idle_screen_off_secs = 900
idle_screen_off_command = "xset dpms force off"
idle_screen_on_command = "xset dpms force on"
```

The stage is off unless both a timeout and a command are set. The on-command
runs when input returns, and is only needed for tools that do not restore
themselves — `xset dpms force off` wakes on its own, `wlopm` does not.

## Caffeine

`toggle_idle_inhibit` holds the session awake until it is toggled back, and the
control center has a **Caffeine** row for the same thing. Switching it on while
the screen is already dim brightens it immediately rather than waiting for the
next input. Either way the flip raises a labeled OSD card (`Caffeine On` /
`Caffeine Off`) — bound to a key with the control center closed, the card is
the only confirmation the flip happened.

Three other things hold the session awake without being asked:

- a client's idle inhibitor (a video player, on the Wayland backend);
- a screen recording in progress — recording an unattended screen is exactly
  when the machine looks idle and must not be treated as such;
- an audio recording in progress, for the same reason.

An inhibitor *wakes* the session rather than freezing it: starting a film while
the screen is already dim brightens it, instead of leaving it dim for the whole
film.

## Where the idle clock comes from

The window manager cannot count this itself — it only receives the events it
grabbed, so a session spent typing into one window would look idle from up
there. So:

| Backend | Idle clock |
| --- | --- |
| `x11rb`, `xcb` | XScreenSaver extension |
| `wayland-udev` | its own input pipeline |
| nested Wayland backends | none — the policy stays out of the way |

A backend with no idle clock is not guessed at: the policy simply does nothing,
because dimming the screen of somebody who is working is worse than never
dimming at all. A clock that stops answering mid-session is treated the same
way, and whatever the policy had already dimmed is put back rather than left
dark — nothing can notice the activity that would otherwise undo it.

### The X server's own blanker is switched off

X11 has a blanking timer of its own, and the two do not merely overlap — they
fight. When the server's blanker fires it resets the very clock this policy
reads, so a lock timeout longer than the server's blanking timeout (600 seconds
on a stock server) would never be reached, and a dim would be undone every ten
minutes for no reason.

The first time the idle policy actually reads the clock, JWM therefore
switches the server's timer off, the same way `xset s off` does, and logs that
it did. Its own stages replace it; `idle_screen_off_command` is how you get
real blanking back. If you would rather keep the server's blanker, set every
idle stage to 0 and JWM will not touch it — and a server whose idle clock JWM
cannot read never reaches that point either, so it keeps its own blanker
instead of ending up with neither.

## Over IPC

```sh
jwm-tool msg get_idle_status
# {"inhibited": false, "dimmed": true, "screen_off": false, "locked": false,
#  "dim_secs": 120, "lock_secs": 600, "screen_off_secs": 900}

jwm-tool msg toggle_idle_inhibit
```

An `idle/state` event carrying the same payload is broadcast on the `idle`
topic whenever anything changes, so a status bar can show a caffeine indicator
without polling.

The three timeouts are the ones the policy will act on, not the numbers in the
file, so a bar counting down to the lock counts down to the lock that actually
happens: `lock_secs` reports the 60-second floor when `behavior.idle_lock_secs`
is set below it, and `screen_off_secs` reports `0` whenever
`idle_screen_off_command` is empty, however `behavior.idle_screen_off_secs` is
set. `0` in any of the three means that stage will not fire. The query is
therefore an answer about behaviour, not a way to read the configuration
back: a panel that wants the numbers as written has to read the file
(`jwm --print-config-path`).

Every timeout is settable live too:

```sh
jwm-tool msg set_config --args '{"key": "behavior.idle_lock_secs", "value": 600}'
```

## Known limitation

The dim applies to the composited desktop, not to JWM's own overlays: a lock
screen or control center drawn while the session is dim renders at full
brightness. It looks slightly odd and costs nothing else.
