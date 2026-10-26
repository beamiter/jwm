# Startup and configuration commands

The `jwm` binary owns process bootstrap only: command-line parsing, logging,
locale setup and D-Bus discovery. It passes an immutable `ApplicationOptions`
snapshot to the application composition root, which selects the backend and
runs the window-manager lifecycle.

## Select a backend

```bash
# Primary production (CLI default when compiled in):
jwm --backend wayland-udev
# X11 compatibility:
jwm --backend x11rb
jwm --backend xcb
# Nested development / CI only:
jwm --backend wayland-x11
jwm --backend wayland-winit
```

The existing `JWM_BACKEND` environment variable remains supported. Command-line
values take precedence when both are present. The short aliases `wayland`,
`udev`, `windowed` and `winit` are accepted for compatibility. When neither
`--backend` nor `JWM_BACKEND` is set, JWM prefers `wayland-udev` if that
feature was compiled into the binary.

Display managers should launch the **Wayland** session entry
(`jwm-wayland.desktop`) for day-to-day use; the X11 session entries remain for
compatibility.

## Inspect and validate configuration

Each backend family has a separate configuration file:

- X11 (`x11rb`, `xcb`): `~/.config/jwm/config_x11.toml`
- Wayland: `~/.config/jwm/config_wayland.toml`

Use the process-safe maintenance commands instead of starting a display server:

```bash
jwm --backend x11rb --print-config-path
jwm --backend wayland --check-config
jwm --update-config
jwm --gen-config
```

`--update-config` checks both templates and is used by the source installer.
It keeps the existing bytes when only values, comments, ordering, or array
lengths differ. Changed fields, including nested fields, cause a backup and
replacement with defaults. Missing files are created; invalid TOML and symbolic
links are kept. A concurrent edit detected before publication stops the update.

`--gen-config` explicitly resets both templates. An existing file is backed up to the matching
`*.toml.backup` path before replacement, with numbered suffixes when earlier
backups exist. A backup is published only after its complete contents are synced.
Writes use a same-directory atomic
replace, preserve an existing configuration symlink, and sync the new file
before returning.

Temporary-file collisions are retried up to 128 times without deleting another
writer's file. Backup publication checks the base `*.toml.backup` name and
suffixes `.1` through `.127`; if all are occupied, generation returns an error
and preserves the configuration and existing backups. Older backups with higher
suffixes remain readable.

Configuration reads, writes, backups and restores share a 4 MiB limit.
Theme and layout persistence refuse edits that would cross it before creating
directories or replacing the existing file. Reads accept regular files;
special files such as FIFOs are rejected without waiting for a writer. Backups
preserve the original bytes even when an interrupted edit left invalid TOML or
UTF-8. Restore atomically replaces the resolved target, preserves existing
permissions and configuration symlinks, and creates missing targets with mode
`0600`.

`--check-config` validates both TOML structure and runtime semantics without
constructing a backend. It reports unreachable shortcut collisions, unknown
keys/functions/modifiers, unsafe tag and geometry ranges, invalid enum-like
values, and media/display invariants. Warnings keep a usable configuration
valid; errors return a non-zero status. Normal startup performs the same
preflight and stops before opening a display when the selected configuration is
syntactically invalid or has a blocking semantic error. It no longer silently
starts with unrelated built-in defaults.

Legacy templates that assigned Alt+Shift+C/S twice are recognized at runtime:
the calculator scratchpad and sticky-window actions move to Alt+Control+C/S when
those target chords are free. The diagnostic remains visible until the file is
updated, so the compatibility migration never hides on-disk ambiguity.

## Keyboard defaults and upgrades

Alt remains the primary modifier. J/K focus, H/L resizing, tag numbers,
Alt+Tab, terminal launch, and close-window bindings retain their existing
meaning. The physical-arrow resize family is the consistent width/height
control; the existing Shift+H/L signs remain compatible with previous files.

| Action | Generated default | Previous default |
| --- | --- | --- |
| Screenshot selection / full desktop | Print / Shift+Print; Alt+S / Alt+Shift+S still available | Alt+S / Alt+Shift+S |
| Start/stop recording | Ctrl+Print; Alt+Ctrl+R still available | Alt+Ctrl+R |
| Adjust active recording source | Ctrl+Shift+Print | Alt+Ctrl+Shift+R |
| Bluetooth picker | Alt+Shift+F10, beside Alt+F10 control center | Alt+Ctrl+Shift+B |
| Toggle tiled/floating | Alt+Ctrl+Space | Alt+middle-click only |
| Toggle maximize, tiled or floating | Alt+Ctrl+Return | Custom binding or floating-only Alt+Shift+Up |
| Log out | Alt+Shift+Escape, select Log Out, then confirm | Immediate Alt+Shift+Q |
| WaterLily layer, case and palette | `scripts/waterlily.sh` | Alt+Shift+F11/F10/F9 |

Print groups capture controls on one key and removes a three-modifier stretch
from recording adjustment. Bluetooth uses a two-modifier control-center
variant. Ctrl+Alt+function keys are reserved for X11 virtual-terminal switching
and are deliberately not used. Direct `quit` remains bindable and scriptable;
the generated key table uses the session menu's existing confirmation.

An existing `[[keybindings.keys]]` list remains authoritative. Upgrading does
not rewrite it or remove explicit WaterLily/custom bindings. To adopt these
defaults, edit the selected backend's file reported by `--print-config-path`:

1. Remove the entries whose functions are `toggle_waterlily`, `waterlily_case`
   and `waterlily_palette`, and remove the direct `quit` entry if adopting
   confirmed logout. Keep `session_menu` on Alt+Shift+Escape.
2. Replace the Bluetooth and recording-adjustment entries with the keys in the
   table. Add Print, Shift+Print, Ctrl+Print and the two window-state bindings
   only if those chords are free. In TOML use `key = "Print"`, `"space"` or
   `"Return"`, with `modifier = ["Control", "Shift"]` for Ctrl+Shift, for
   example. Their functions are `take_screenshot`, `take_screenshot_fullscreen`,
   `toggle_recording`, `adjust_recording_region`, `togglefloating` and
   `togglemaximize`; their argument is `0`.
3. Run `jwm --backend <backend> --check-config`, then let the normal live
   reload apply the edit. Replace old entries rather than appending duplicate
   shortcuts. On a keyboard without Print, keep a custom recording-adjustment
   key and the existing Alt letter capture keys.

The optional chord prefix remains opt-in. Its second key uses an exact
modifier match: release the leader's modifier if the second binding omits it,
then press the modifiers the second binding declares. Preparing Ctrl/Shift
does not cancel the sequence or extend its timeout. Alt+Shift+/ shows these
modifiers and only the tag numbers that actually have generated bindings.

## Run the startup doctor

The doctor is read-only: it does not generate configuration, open a display,
or probe DRM devices by taking control of them.

```bash
jwm --backend wayland-udev --doctor
jwm --backend wayland-udev --doctor --json
jwm --backend x11rb --doctor
```

It checks the selected configuration, status-bar and `jwm-tool` executables,
runtime-directory ownership and permissions, display/DRM prerequisites, and
the D-Bus session environment. JSON output has a versioned schema and is useful
for installers and support bundles. Warnings return success; a blocking error
returns a non-zero status.

Treat a green `wayland-udev` doctor run as the **minimum daily-drive gate**
before calling a machine production-ready. Hosted CI cannot certify
kernel/GPU/driver combinations — complete
[hardware validation](hardware-validation.md) on real hardware.

## Inspect a running instance

The startup doctor intentionally remains offline and read-only. Once JWM is
running, use the backend-neutral health query for live state:

```bash
jwm-tool health
jwm-tool health --json
jwm-tool capabilities
```

The health JSON schema starts at version 1 and reports the backend captured from
the selected `ApplicationOptions`, not an inferred environment value. It also
contains uptime, configuration/reload health, window/monitor/workspace counts,
active feature states, and compositor metrics when available. Capabilities lists
the commands, queries, and subscription topic prefixes supported by that binary.
Unsupported optional compositor metrics are represented as `null`; they do not
make an otherwise healthy instance degraded.

`jwm-tool status` is preserved for compatibility and continues to query the
optional JWM supervisor daemon. It is distinct from the live `health` command.

## Benchmark a compositor backend

```bash
jwm --backend wayland-udev --benchmark 600 --benchmark-warmup 60
```

The benchmark starts after JWM setup, excludes the warm-up frames and requests
automatic exit when the sample is complete; the JSON report goes to stdout. The
compatibility environment variables `JWM_BENCHMARK` and `JWM_BENCHMARK_WARMUP`
remain available.

The X11 compositor (`x11rb`, `xcb`) and `wayland-udev` share one harness and
one report schema. Only rendered frames count, and a calm desktop renders
nothing, so give the run a steady workload (see
[performance](performance.md)) or it waits for frames that never come. The
nested Wayland backends have no compositor harness and refuse the request.

All startup and IPC benchmark requests use the same resource limits: measured
frames must be in `1..=100000`, and warm-up frames in `0..=10000`. These limits
bound the compositor's in-memory sample buffers and prevent accidentally
starting an effectively unbounded run. At 60 Hz, the maxima are approximately
28 minutes of measurement and 2.8 minutes of warm-up.

## Logging

```bash
jwm --log 'info,jwm=debug,smithay=warn'
```

`--log` uses the same filter syntax as `RUST_LOG`. Without an explicit filter,
JWM keeps its own diagnostics useful while reducing noisy third-party logs.

## Restart behavior

A JWM restart re-executes the current executable with the original OS-native
arguments. This preserves command-line backend and benchmark options, supports
non-UTF-8 argument values and avoids rebuilding an argument vector on every
in-process fallback restart.

`jwm-tool health` prints compact `wayland` for startup operators.
