# Session menu and night light

## Session menu

`Alt+Shift+Escape` (`session_menu`) opens the power actions as a material
card: lock, suspend, hibernate, log out, restart, shut down. `Up`/`Down` move,
`Enter` selects, `Esc` — or `Alt+Shift+Escape` again — closes. The control
center's `Session…` row opens the same panel.

Hibernate is listed only when the kernel advertises suspend-to-disk in
`/sys/power/state`, so the row is never an action that cannot work.

### Confirmation

Log out, restart, and shut down need **two** presses of `Enter`: the first
arms the row and it says `Enter to confirm`, the second runs it. Moving the
selection or pressing `Esc` cancels the arming. Suspend and hibernate run on
the first press — a key wakes the machine back up, so there is nothing to
protect against.

### What each action does

| Action | Behavior |
| --- | --- |
| Lock Screen | Swaps in JWM's built-in lock overlay, keeping the input grabs |
| Suspend | `behavior.suspend_command`, default `systemctl suspend` |
| Hibernate | `behavior.hibernate_command`, default `systemctl hibernate` |
| Log Out | Quits the window manager, ending the session |
| Restart | `behavior.reboot_command`, default `systemctl reboot` |
| Shut Down | `behavior.shutdown_command`, default `systemctl poweroff` |

The configured commands are **argv, not shell lines**: they are split on
whitespace and executed directly, so a stray quote cannot turn into a second
command. Point them at `loginctl`, a `doas` wrapper, or anything else your
session allows:

```toml
[behavior]
suspend_command = "loginctl suspend"
shutdown_command = "loginctl poweroff"
```

A command that fails leaves the menu open with the error logged, rather than
dropping you onto a bare desktop wondering whether anything happened.

## Night light

JWM already warms the screen on a schedule (`behavior.night_light`,
`night_light_start`, `night_light_end`, `night_light_temp`,
`night_light_transition_mins`), applied once a minute.

`toggle_night_light` and the control center's **Night Light** row add a manual
override on top of that schedule. The override wins until it is toggled back,
so warmth at noon stays on and a bright screen at midnight stays bright. It
applies immediately rather than waiting for the next schedule tick, and is
broadcast as `night_light/toggle` on the `night_light` topic.

The override is in-memory: a restart returns to the configured schedule.

## IPC

`session_menu`, `toggle_night_light`, `control_center`, and
`notification_center` are all dispatchable over IPC as well as bindable:

```sh
jwm-msg '{"command": "session_menu"}'
jwm-msg '{"command": "toggle_night_light"}'
```
