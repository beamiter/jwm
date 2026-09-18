# Window placement

Where a new window lands is decided once, when JWM starts managing it, in
this order:

1. A transient window (a dialog with `WM_TRANSIENT_FOR`) follows its parent's
   monitor and tags and floats.
2. Everything else starts on the selected monitor, on that monitor's active
   tags: wherever the pointer is.
3. A matching `[[rules]]` entry can pin the tags, the monitor, or both.
4. The closed-placement memory, described below, moves the window to where
   the same application was last closed, unless a rule already pinned that
   part or JWM launched the window itself.

Step 2 is the right default for a window the user asked for with a
keybinding or from the launcher: they acted on this monitor, on this tag,
and expect the window here. It is the wrong default for most other windows.
A browser reopened from a shell, a viewer that an agent (`claude`, `codex`)
spawns from inside a terminal, an application's own second top-level: each
of those has a place it was last seen, and step 4 sends it back there.

```toml
[behavior]
remember_closed_placement = true   # default
```

The key is hot-reloadable and accepts `set_config`. Switching it off also
forgets everything remembered so far, so a later re-enable starts from what
the user does next rather than from stale history.

## What is remembered

When a regular top-level window is unmanaged, closed by the user or by its
process, JWM records its monitor number and tag mask under its `WM_CLASS`
(class and instance, matched exactly). The newest close of an identity
replaces the previous one. Status bars, docks, scratchpads, sticky windows,
transients and popup-like window types (dialogs, tooltips, notifications,
splash screens, utility windows) are never recorded.

The memory lives in the JWM process. It holds up to 256 identities, evicts
the oldest when full, and does not survive a restart of JWM.

## Who gets the memory

The exclusion is about who launched the window, not what it is. JWM records
the PID of every child it spawns itself: keybinding `spawn` commands, the
application launcher, scratchpads, the session menu and other shell panels.
When a new window maps, JWM walks its process ancestry and stops at the
first process it recognises:

| First recognised process on the chain | Origin | Memory |
| --- | --- | --- |
| another managed window's process | launched from a window: a terminal, an agent inside one, or the application's own second window | applied |
| a child JWM spawned | keybinding, launcher, scratchpad, shell panel | not applied |
| none (a tmux server, a daemon, an SSH session, a D-Bus service) | external | applied |

A managed window's process wins over a spawn record at every step, so an
application started inside a terminal that JWM spawned is the terminal's
doing, not JWM's. A spawn record is matched by PID and, when readable,
by the process start time, so a recycled PID cannot claim it.

Two situations give no chain to walk. A D-Bus-activated application (for
example `gnome-terminal`, whose spawned wrapper exits while a long-lived
server maps the window) reaches nothing JWM knows, and Wayland backends
currently report no PID at all. In both cases the window is blamed on the
newest JWM spawn that has not produced a window yet, provided that spawn is
at most ten seconds old, and each spawn can be blamed once. Outside that
window such a window counts as external and gets the memory.

## Where the window goes

The remembered monitor and tags are applied after rules. A rule that pins
tags keeps its tags; a rule that pins a monitor keeps its monitor; the memory
fills in only what the rule left open. If the remembered monitor is no
longer connected, the window stays on the selected monitor and only the
remembered tags apply, because tags are the user's workspaces and travel
with them across outputs.

A window placed by the memory is then shown: if its tag is not active on
its monitor, that monitor switches to the tag, with the usual tag-switch
transition. On another monitor the view switches there as well, and whether
keyboard focus follows is decided by `behavior.focus_follows_new_window`,
exactly as for any other new window on another monitor.

## Diagnostics

Every decision is logged under the `[closed-placement]` prefix: a close
that was remembered, a window that was launched by JWM and kept its default
placement, and a window that returned to its remembered monitor and tags
together with the origin the chain resolved to.
