# Focused-tag placement regression audit

Base: a7f95f050727dd14406d01ee06ca34b6da4cf14b (master inspected for the 2026-10-20 request).

## Scope and findings

Source review found no confirmed production defect in the default switch.
BehaviorConfig uses serde's false default for an omitted remember_closed_placement field;
Config::default also sets false. Explicit booleans survive TOML serialization.
The common manage path assigns the selected monitor and applies rules before optional
closed-placement memory. Managed transient parents supply monitor/tags and float state
without applying closed-placement memory.

File reload and IPC set_config both reach apply_config_changes, which reconciles memory.
Disabling clears the in-memory map and, in non-test builds, attempts to delete the snapshot.
Startup loads the snapshot only when enabled; disabled startup deletes/ignores it.
If deleting a snapshot fails, it remains ignored while disabled, but a subsequent process
started with memory enabled can load that file. This is a filesystem-error limitation,
not evidence of a successful invalidation. Existing live-window session restoration is
separate from closed-application placement and should remain enabled across restart.

## Added regression cases (not executed in this environment)

- Omitted field, built-in default, explicit true/false and TOML round-trip.
- Repeated set_value transitions and rejection of a string masquerading as a boolean.
- Reopen from tag 3 onto focused tag 7, on the same and a different monitor,
  using both the real default and a per-instance explicit false fixture.
- Terminal-origin window with retained stale history while disabled; subsequent
  reconciliation clears that history.
- Dialog follows its parent on another monitor/tag with memory enabled and disabled.

Existing tests cover opt-in external/terminal launches, JWM-origin exclusion,
remembered-tag focus/urgency, rule tag/monitor precedence, connector remapping,
snapshot v1 compatibility and disk round-trip/delete, and off/on memory reset.
The new tests deliberately reuse those fixtures without mutating global CONFIG.

## Validation actually performed

- Retrieved and inspected current master and the shared placement/startup/reload paths.
- Rustfmt successfully parsed the modified Rust files. Unrelated formatting changes
  were removed from the patch.
- git diff --check passed.
- Installed rustc/cargo 1.99.1 and attempted:
  cargo test -p jwm --lib closed_placement --no-default-features
- Cargo stopped during dependency download with "No space left on device (os error 28)".
  The container's 32 GB root filesystem was full. No Rust test reached execution.
- Xvfb exists, but no built JWM binary or active desktop session was available.
  No X11 or Wayland integration result is claimed.

## Follow-up commands on a build host

Run the shared unit tests (these use a backend spy, not real display protocols):

```sh
cargo test -p jwm --lib --no-default-features closed_placement
cargo test -p jwm --lib --no-default-features disabled_memory
cargo test -p jwm --lib --no-default-features transient_parent_placement
cargo test -p jwm --lib --no-default-features a_window_
cargo test -p jwm --lib --no-default-features switching_the_feature_off
cargo test -p jwm --lib --no-default-features the_memory_fills
cargo check -p jwm --no-default-features --features backend-x11rb
cargo check -p jwm --no-default-features --features backend-wayland-nested
```

## Required desktop matrix

Use isolated XDG config/state directories and two outputs. Repeat on X11 and native
Wayland (plus XWayland when available). Inspect actual window monitor/tag placement,
focused window, active tags and closed_placement.json after each operation.

| Scenario | Expected |
| --- | --- |
| Key omitted / false: close on output B tag 3; reopen with A tag 7 focused | New window on A tag 7 |
| Same scenario with true, external or terminal-origin launch | Remembered B tag 3; no focus/view theft |
| true, JWM keybinding/launcher-origin launch | Current focused placement |
| Matching tag-only / monitor-only / both rule | Rule-pinned components win; enabled memory only fills unpinned components |
| Dialog with managed parent on another output/tag | Parent placement and floating state in both modes |
| true -> false by file reload and separately IPC | Map and snapshot cleared; subsequent closes not recorded |
| false -> true in same process | Old history does not return; later closes are learned |
| Restart with omitted / false and a preseeded historical snapshot | Snapshot ignored/deleted; newly launched application uses current focus |
| Restart with true and a valid snapshot | Snapshot restored; explicit opt-in preserved |
| Failed snapshot deletion | Disabled mode ignores history; inspect error and retained file before enabled restart |
| Live windows across JWM restart | Existing session state preserved, distinct from newly launched windows |

TOML round-trip and direct reconcile tests do not prove the file watcher, IPC transport,
real process ancestry, compositor focus behavior, or a real seamless restart.
