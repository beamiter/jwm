# Idle

When nobody has touched the machine for a while, JWM dims the screen, and — if
you ask it to — locks it and powers the displays down. Any input undoes the
dim; only the password dismisses the lock.

While locked, `Backspace` removes one character and `Esc` securely clears the
whole entered password plus any previous authentication error; neither key
unlocks or closes the surface.

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

- **A floor of 30 seconds.** A non-zero `idle_lock_secs` below 30 is raised to
  30, with one warning in the log. `idle_lock_secs = 1` otherwise re-locks
  between the keystrokes of the password, and the only way back in is editing
  the config from behind the lock screen. `0` still switches the stage off
  outright.
- **A minute of grace after every unlock.** Typing the password is a statement
  that somebody is at the keyboard, so the lock stage does not re-arm for 60
  seconds afterwards. Dimming and the screen-off stage are unaffected.

If a lock cannot be shown — something else holds the pointer grab, a menu is
open — the attempt is retried every 5 seconds rather than abandoned for the
rest of the idle period, so a passing grab does not silently leave an
unattended desk unlocked. The first failure is a warning in the log and the
repeats are at debug level.

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
next input.

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
dimming at all.

### The X server's own blanker is switched off

X11 has a blanking timer of its own, and the two do not merely overlap — they
fight. When the server's blanker fires it resets the very clock this policy
reads, so a lock timeout longer than the server's blanking timeout (600 seconds
on a stock server) would never be reached, and a dim would be undone every ten
minutes for no reason.

The first time the idle policy runs, JWM therefore switches the server's timer
off, the same way `xset s off` does, and logs that it did. Its own stages
replace it; `idle_screen_off_command` is how you get real blanking back. If you
would rather keep the server's blanker, set every idle stage to 0 and JWM will
not touch it.

## Over IPC

```sh
jwm-tool msg get_idle_status
# {"inhibited": false, "dimmed": true, "screen_off": false, "locked": false,
#  "dim_secs": 120, "lock_secs": 600, "screen_off_secs": 900}

jwm-tool msg toggle_idle_inhibit
```

An `idle/state` event carrying the same payload is broadcast on the `idle`
topic whenever anything changes, so a status bar can show a caffeine indicator
without polling. Every timeout is settable live too:

```sh
jwm-tool msg set_config --args '{"key": "behavior.idle_lock_secs", "value": 600}'
```

## Known limitation

The dim applies to the composited desktop, not to JWM's own overlays: a lock
screen or control center drawn while the session is dim renders at full
brightness. It looks slightly odd and costs nothing else.
