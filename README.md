# JWM

JWM is a Rust window manager and compositor with native X11 and Wayland
backends. It combines tag-based tiling, multiple layouts, multi-monitor control,
animations and compositor effects with a JSON IPC control plane. The project is
under active development and supports both direct DRM/KMS sessions and nested
development backends.

## Highlights

- X11RB and XCB window-manager backends with an integrated X11 compositor.
- Direct Wayland DRM/KMS, nested X11, and nested winit backends with XWayland.
- Tile, monocle, floating, scrolling, grid, deck, fibonacci, centered-master,
  bstack, three-column, tatami, fullscreen, and vertical-stack layouts.
- Tags, per-monitor state, overview/expose, display layout UI, screenshots,
  screen/audio recording, session restore, gestures, accessibility filters,
  HDR/VRR/color-management plumbing, and direct-scanout diagnostics.
- Full-screen WaterLily.jl simulation frames on the X11RB/XCB compositor,
  produced externally on CPU, CUDA, or ROCm.
- Live configuration reload and a newline-delimited JSON IPC API exposed through
  `jwm-tool`.
- Read-only startup health checks, semantic configuration diagnostics, and
  privacy-aware support bundles.

## Build and verify

JWM requires the normal Linux X11, Wayland, DRM/GBM, libinput, libseat, EGL/GL,
ALSA, D-Bus, and font/rendering development packages for your distribution.
The minimum supported Rust version with the committed `Cargo.lock` is 1.89;
it is declared in `Cargo.toml` and checked in CI.

On a fresh Debian/Ubuntu machine, `scripts/bootstrap_deps.sh` installs every
native dependency plus the Rust toolchain (via rustup, since distro packages are
older than the 1.89 floor) in one step:

```bash
bash scripts/bootstrap_deps.sh              # apt packages + rustup toolchain
JWM_CN_MIRROR=1 bash scripts/bootstrap_deps.sh   # China: rustup + cargo via rsproxy.cn
bash scripts/bootstrap_deps.sh --help       # options: --no-apt, --no-rust, --with-portal, --cn
```

`--with-portal` (or `JWM_WITH_PORTAL=1`) adds the PipeWire headers needed by the
screencast portal. On non-Debian distros the script prints the required library
groups to map to your package manager. Then build:

```bash
cargo build --locked --release
cargo test --locked --lib --bins --tests
```

The release build produces `jwm`, `jwm-tool`, and `jwm-support`. Before starting
a display backend, inspect the environment and configuration:

```bash
target/release/jwm --backend x11rb --doctor
target/release/jwm --backend wayland-udev --doctor --json
```

## Configure and run

X11 and Wayland use separate files under `~/.config/jwm`:

```bash
target/release/jwm --gen-config
target/release/jwm --backend x11rb --check-config
target/release/jwm --backend wayland --check-config

target/release/jwm --backend x11rb
# Direct DRM/KMS session:
target/release/jwm --backend wayland-udev
```

Supported backend names are `x11rb`, `xcb`, `wayland-udev`, `wayland-x11`, and
`wayland-winit`. See [startup and configuration](docs/startup.md) for aliases,
logging, benchmarking, restart behavior, and doctor output.

The installation helper builds JWM and one selectable status bar, installs the
session files, and keeps existing configuration unless `--gen-config` is used:

```bash
scripts/install_jwm_scripts.sh --help
```

## Control JWM

`jwm-tool` sends typed JSON commands and queries over JWM's private Unix socket:

```bash
jwm-tool msg get_windows
jwm-tool msg view --args '{"tag":2}'
jwm-tool msg setlayout --args '{"layout":"scrolling"}'
jwm-tool msg spawn --args '{"cmd":["alacritty"]}'
jwm-tool msg '' --subscribe 'window,tag,layout'
jwm-tool health
jwm-tool health --json
jwm-tool capabilities --json
```

Malformed JSON, invalid argument types, overflow, empty spawn commands, unknown
commands, and `{ "success": false }` responses produce a non-zero exit status,
so the tool is safe to use from scripts.

`health` is a backend-neutral live snapshot of the running JWM instance. Its
versioned JSON includes the actual selected backend, uptime, configuration
health, window/monitor/workspace counts, active features, and compositor metrics
when the backend exposes them. `capabilities` discovers the supported IPC
commands, queries, and subscription topics. The older `jwm-tool status` command
retains its existing meaning: it reports the optional process supervisor rather
than querying JWM's live IPC socket.

`save_session` writes a private, atomic snapshot under
`$XDG_STATE_HOME/jwm/session.json` (normally
`~/.local/state/jwm/session.json`); restore also recognizes the legacy cache
location. `restore_session` reapplies monitor, tag, and floating-window state.

## Create a support bundle

`jwm-support` combines the offline startup doctor with optional live health and
capability queries in a versioned JSON document:

```bash
jwm-support --backend x11rb --output jwm-support.json
jwm-support --backend wayland-udev --offline --output jwm-support.json
jwm-support --strict --compact > jwm-support.json
```

File output is private (`0600`) and atomically replaced. The collector uses a
small environment allowlist and redacts configuration, executable, runtime,
and IPC error details; it excludes HOME, PATH, D-Bus addresses, process command
lines, window titles, and arbitrary environment variables. Review
[support bundles](docs/support-bundles.md) before attaching a
report to a public issue.

The default modifier is Alt (`Mod1`). Useful built-in bindings include:

| Binding | Action |
| --- | --- |
| Alt+Shift+Return | Launch terminal |
| Alt+R | Application launcher |
| Alt+Control+Escape | Lock screen |
| Alt+Control+O | Display layout |
| Alt+S / Alt+Shift+S | Interactive / immediate desktop screenshot |
| Alt+Control+R | Interactively choose a source and start/stop screen recording |
| Alt+Control+Shift+R | Move, resize, or replace the active recording source |
| Alt+Shift+C | Close focused client |
| Alt+Control+C | Calculator scratchpad |
| Alt+Control+S | Toggle sticky window |
| Alt+Shift+F11 | Toggle the WaterLily simulation |
| Alt+Shift+F10 | Cycle the WaterLily simulation case |
| Alt+Shift+/ | Show all bindings |

During interactive screenshot or recording selection, press `G`, `W`, `M`, or
`D` to choose a dragged region, a window, the monitor under the pointer, or the
entire desktop. `Tab` and `Shift+Tab` cycle the same choices. Window capture
shows a hover preview and is confirmed with the left mouse button; `Enter`
saves a screenshot or starts/commits recording. Arrow keys nudge a committed
selection (`Shift` uses 10-pixel steps), while `Escape`, right-click, or the
recording shortcut again cancels safely.

## Portal and diagnostics

The optional `portal/` crate provides JWM's screencast portal backend. Its
installer builds the independent manifest, installs a per-user D-Bus activation
service with the correct home path, and restarts an older activated backend:

```bash
scripts/install-portal.sh
scripts/test-portal.sh
```

Portal builds require PipeWire 1.2 development files, `pkg-config`, and libclang.
System installations are discovered automatically. For a private PipeWire
prefix, set `JWM_PIPEWIRE_PREFIX`; the installer derives the pkg-config search
path and runtime rpath, and it also honors `CARGO_TARGET_DIR`:

```bash
JWM_PIPEWIRE_PREFIX=/opt/pipewire-1.2 scripts/install-portal.sh
```

Additional operational tools are documented in [tools/README.md](tools/README.md).
The external Julia simulation worker and frame protocol are documented in
[docs/waterlily.md](docs/waterlily.md).
Architecture boundaries and the incremental migration plan are in
[docs/architecture.md](docs/architecture.md). The delivery sequence for larger
changes is tracked in [the evolution roadmap](docs/roadmap.md).

## Contributing and security

Before opening a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md). Please
report security-sensitive problems through the private process described in
[SECURITY.md](SECURITY.md), not a public issue.
