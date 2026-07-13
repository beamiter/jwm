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
`*.toml.backup` path before replacement. `--check-config` validates TOML syntax
and the deserialized configuration structure without constructing a backend.

## Benchmark a compositor backend

```bash
jwm --backend wayland-udev --benchmark 600 --benchmark-warmup 60
```

The benchmark starts after JWM setup, excludes the warm-up frames and requests
automatic exit when the sample is complete. The compatibility environment
variables `JWM_BENCHMARK` and `JWM_BENCHMARK_WARMUP` remain available.

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
