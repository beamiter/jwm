# Exchange complete monitor workspaces

`swapmon` exchanges the current monitor's entire set of tags with another
monitor. The keyboard focus and pointer stay on the current physical output,
so a remote desktop showing only that output can bring another monitor's
content into view without switching the remote client's display.

With two monitors, press **Alt+Ctrl+Shift+.** to exchange their content; press
it again to exchange it back. **Alt+Ctrl+Shift+,** chooses the previous monitor.
With three or more monitors these keys choose the next/previous unlocked output
in monitor order. Holding a key does not repeatedly exchange the monitors.

Both sides transfer together:

- Every application's monitor ownership, including windows on inactive tags,
  multi-tag windows, sticky/PiP windows, minimized windows, and swallowed parents.
  A window keeps its tag mask and minimized state.
- The current and previous tag views, per-tag focus, current/previous layouts,
  master counts and fractions, gaps, and per-tag bar visibility preferences.
- Application layout order, application focus-stack order, scrolling columns,
  focused columns/windows, column widths, saved viewports on every tag, and
  layout-specific manual resize intentions.

Monitor numbers, output identities, positions, modes, scaling, and physical
bar/dock/desktop surfaces stay on their outputs. Floating positions and restore
rectangles translate relative to the destination work area and clamp to fit a
smaller output. Fullscreen windows fill their new output; maximized windows
refit its work area. In particular, normal windows in a FLOAT layout move even
when their individual floating flag is off. Unassigned parked scratchpads stay
unassigned; monitor-owned scratchpads travel with their workspace.

The usual layout persistence and restart snapshots see the exchanged state.
External launch rules and hardware configuration are not rewritten. The
existing bindings for focusing a different monitor or moving one window are
unchanged. Older configuration files gain the new shortcuts in memory only
when those keys are free and the action has no explicit binding or chord.
They appear in the keybinding viewer as “exchange all monitor workspaces”.

## IPC and custom bindings

```bash
# Exchange with the next unlocked monitor (default direction).
jwm-tool msg swapmon

# Exchange with the previous unlocked monitor.
jwm-tool msg swapmon --args '{"direction":-1}'

# Use the monitor id reported by get_monitors, not its position in a list.
jwm-tool msg get_monitors
jwm-tool msg swapmon --args '{"monitor":2}'
```

`swap_monitors` is an alias. Direction also accepts the existing `value`/`v`
form, such as `--args '{"value":1}'`. Use exactly one argument: a direction
(-1, 0, or 1), or a non-negative monitor id. Direction 0, an identical target,
or a single-monitor setup is a no-op; an unknown explicit id is an error.

For a custom `[[keybindings.keys]]` entry, set `function = "swapmon"` and use
an integer `argument` of -1 or 1. For a fixed target, bind a `spawn` command
that calls the explicit-monitor IPC form above.

The IPC acknowledgement follows the completed state exchange. Subscribers to
`monitor` receive `monitor/swap` with `source` and `target` monitor ids, alongside
the ordinary monitor-bar and window-state updates. `tag/view` reports the
incoming view on the still-selected monitor. Output enumeration and monitor
focus do not change.

Locked outputs cannot participate; directional selection skips them. A session
lock or active window/tab/wallpaper drag, screenshot interaction, expose,
peek, or 3D overview refuses the exchange before changing state. A layout
preview is cancelled first and other open shell panels close, so a later panel
cancel cannot restore a layout to the wrong monitor.

## Validation

```bash
scripts/test.sh --lib monitor_swap
scripts/test.sh --lib jwm::monitor::
scripts/test.sh --lib jwm::tag_manager::
scripts/test.sh --lib config::tests::
scripts/test.sh --lib ipc::tests::
cargo build --locked --bin jwm
python3 scripts/test_monitor_swap.py
JWM_SWAP_TEST_BACKEND=xcb python3 scripts/test_monitor_swap.py
# Optional real compositor path, using GLX and Mesa software rendering in Xvfb:
LIBGL_ALWAYS_SOFTWARE=1 JWM_SWAP_TEST_COMPOSITOR=1 python3 scripts/test_monitor_swap.py
LIBGL_ALWAYS_SOFTWARE=1 JWM_SWAP_TEST_COMPOSITOR=1 JWM_SWAP_TEST_BACKEND=xcb python3 scripts/test_monitor_swap.py
```

The tests cover complete per-tag state, both directions, repeated exchange,
untouched third outputs, negative origins and different resolutions, empty and
single-monitor cases, stale focus, unlisted clients, physical shell clients,
minimized/fullscreen/maximized/PiP restore geometry, FLOAT layout windows,
locked monitors, shortcut conflicts and IPC argument validation. The Python
regression starts a private Xvfb with two RandR monitors and the built JWM. It
checks inactive-tag layouts, minimized state, native keyboard focus, unchanged
pointer position, rendered window colors on the original output, IPC exchange,
and the global shortcut to exchange back. Its config, state, session bus and
display are isolated from the user's desktop. Hardware Wayland/X11 acceptance
should additionally check status bars, remote-view continuity, and seamless
restart after an exchange.
