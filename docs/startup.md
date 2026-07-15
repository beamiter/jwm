# Startup and configuration commands

The `jwm` binary owns process bootstrap only: command-line parsing, logging,
locale setup and D-Bus discovery. It passes an immutable `ApplicationOptions`
snapshot to the application composition root, which selects the backend and
runs the window-manager lifecycle.

## Select a backend

```bash
jwm --backend x11rb
jwm --backend xcb
jwm --backend wayland-udev
jwm --backend wayland-x11
jwm --backend wayland-winit
```

The existing `JWM_BACKEND` environment variable remains supported. Command-line
values take precedence when both are present. The short aliases `wayland`,
`udev`, `windowed` and `winit` are accepted for compatibility.

## Inspect and validate configuration

Each backend family has a separate configuration file:

- X11 (`x11rb`, `xcb`): `~/.config/jwm/config_x11.toml`
- Wayland: `~/.config/jwm/config_wayland.toml`

Use the process-safe maintenance commands instead of starting a display server:

```bash
jwm --backend x11rb --print-config-path
jwm --backend wayland --check-config
jwm --gen-config
```

`--gen-config` writes both templates. An existing file is copied to the matching
`*.toml.backup` path before replacement. Writes use a same-directory atomic
replace, preserve an existing configuration symlink, and sync the new file
before returning.

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

## Run the startup doctor

The doctor is read-only: it does not generate configuration, open a display,
or probe DRM devices by taking control of them.

```bash
jwm --backend x11rb --doctor
jwm --backend wayland-udev --doctor --json
```

It checks the selected configuration, status-bar and `jwm-tool` executables,
runtime-directory ownership and permissions, display/DRM prerequisites, and
the D-Bus session environment. JSON output has a versioned schema and is useful
for installers and support bundles. Warnings return success; a blocking error
returns a non-zero status.

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
automatic exit when the sample is complete. The compatibility environment
variables `JWM_BENCHMARK` and `JWM_BENCHMARK_WARMUP` remain available.

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
