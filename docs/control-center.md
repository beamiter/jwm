# Control center

`Alt+F10` (`control_center`) opens the shell's quick-settings card. It is
keyboard-driven: `Up`/`Down` move between rows, `Left`/`Right` adjust,
`Enter` toggles or activates, `Esc` closes.

Rows appear only when the machine can back them, so a desktop with no
battery, no backlight, and no player shows a short panel rather than dead
controls.

| Row | Appears when | Keys |
| --- | --- | --- |
| Media | An MPRIS player is running | `Left`/`Right` skip, `Enter` play/pause |
| Network | A wireless radio exists (`nmcli` or `rfkill`) | `Enter` toggles the radio |
| Bluetooth | A controller exists (`bluetoothctl` or `rfkill`) | `Enter` toggles power |
| Volume | `wpctl`, `pactl`, or `amixer` works | `Left`/`Right` adjust, `Enter`/`m` mute |
| Brightness | `brightnessctl` or `/sys/class/backlight` | `Left`/`Right` adjust |
| Battery | A `power_supply` device of type `Battery` exists | read-only |
| Power Profile | `powerprofilesctl` or ACPI `platform_profile` | `Left`/`Right` cycle |
| Night Light | always | `Enter` toggles |
| Do Not Disturb | always | `Enter` toggles |
| Lock Screen | always | `Enter` locks |
| Session… | always | `Enter` opens the [session menu](session-menu.md) |

The panel rebuilds itself when the state behind a row changes — a track
change, a battery poll — so an open card never shows a stale value, and the
selection stays put (clamped if a row disappeared).

## Network and Bluetooth

`nmcli` is preferred and falls back to `rfkill`; whichever answers first is
cached for the session. With NetworkManager the row names the active
connection and its signal strength; with only `rfkill` it can honestly report
just the radio switch, so it says on/off without claiming to know the network.

A wired link is reported when there is no wireless one, and keeps its own icon
even while the Wi-Fi radio is off — an Ethernet cable is still the connection.
Bridges, tunnels, and loopback are never treated as "connected".

Bluetooth uses `bluetoothctl show`, falling back to `rfkill`. Without a
controller the row is hidden rather than shown dead.

Both toggles re-read the hardware afterwards instead of assuming success: a
hard-blocked radio (a physical switch) refuses to come back on, and the row
must show that.

`toggle_wifi` and `toggle_bluetooth` are also bindable and dispatchable over
IPC; they report which tools were missing rather than failing silently.

## Battery

The battery is read from `/sys/class/power_supply`, polled every 30 seconds on
the compositor's frame tick. Both flavors of reporting are handled:
`energy_now`/`power_now` (µWh/µW) and `charge_now`/`current_now` (µAh/µA). The
row shows the level, a level-matched icon (a bolt while charging), and the
estimated time to empty or full — omitted when the hardware reports no usable
rate, and dropped rather than shown when the arithmetic produces something
absurd (more than 48 hours).

`Not charging` — plugged in and holding at a charge limit — counts as full,
not as draining.

### Low-battery warnings

Crossing 20%, 10%, or 5% while discharging posts one notification through the
same path as any other ([notifications](notifications.md)); 5% is critical
urgency. Each threshold warns once: hovering at 19% does not repeat, and the
battery must climb 5 points back above a threshold before it can warn again.
Charging clears the memory, so unplugging later warns afresh.

## Power profiles

`powerprofilesctl` is preferred, falling back to
`/sys/firmware/acpi/platform_profile`; whichever answers first is cached for
the session. `Left`/`Right` cycle through the driver's own profile list and
wrap, and the row re-reads afterwards so it shows what actually took effect
rather than what was requested.

## IPC

```sh
jwm-msg '{"query": "get_power_status"}'
jwm-msg '{"command": "set_power_profile", "args": {"profile": "power-saver"}}'
jwm-msg '{"query": "get_connectivity"}'
jwm-msg '{"command": "toggle_wifi"}'
```

`get_power_status` reports the battery and the available/active profiles.
`set_power_profile` rejects a name the driver does not offer, listing what it
does. The `power` subscription topic carries `power/battery` (only when the
reading actually changed) and `power/profile`; the `network` topic carries
`network/status`, likewise only on a real change.
