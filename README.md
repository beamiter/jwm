# JWM

JWM is a Rust window manager and compositor with native X11 and Wayland
backends. It combines tag-based tiling, multiple layouts, multi-monitor control,
animations and compositor effects with a JSON IPC control plane. The project is
under active development and supports both direct DRM/KMS sessions and nested
development backends.

> **Release status:** JWM has not published a stable release. The manifest
> version identifies development builds, not a production-support commitment.
> See [compatibility](docs/compatibility.md), the tested
> [upgrade/rollback lifecycle](docs/upgrade.md), and the maintainer
> [release process](docs/release-process.md).

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
- Authenticated JWM-to-JWM remote viewing and XTEST control for trusted X11
  LANs, through `jwm-remote` (x11rb and xcb sessions).
- Read-only startup health checks, semantic configuration diagnostics, and
  privacy-aware support bundles.

## Build and verify

JWM requires the normal Linux X11, Wayland, DRM/GBM, libinput, libseat, EGL/GL,
ALSA, D-Bus, and font/rendering development packages for your distribution.
The built-in MP4 recorder additionally requires the `ffmpeg` and `ffprobe`
executables (the `ffmpeg` package on Debian/Ubuntu).
The minimum supported Rust version with the committed `Cargo.lock` is 1.89;
it is declared in `Cargo.toml` and checked in CI.

On a fresh Debian/Ubuntu machine, `scripts/bootstrap_deps.sh` installs every
native dependency plus the Rust toolchain (via rustup, since distro packages are
older than the 1.89 floor) in one step:

```bash
bash scripts/bootstrap_deps.sh              # apt packages + rustup toolchain
JWM_CN_MIRROR=1 bash scripts/bootstrap_deps.sh   # China: rustup + cargo via rsproxy.cn
bash scripts/bootstrap_deps.sh --help       # options include --with-portal, --with-tauri, --cn
```

`--with-portal` (or `JWM_WITH_PORTAL=1`) adds the PipeWire headers needed by the
screencast portal. On non-Debian distros the script prints the required library
groups to map to your package manager. Then build:

```bash
cargo build --locked --release
cargo test --locked --lib --bins --tests
```

The release build produces `jwm`, `jwm-tool`, `jwm-support`, and `jwm-remote`.
Before starting a display backend, inspect the environment and configuration:

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

Native bars need only the default bootstrap dependencies. Selecting a Tauri web
bar also needs the Tauri 2 Linux libraries (`bootstrap_deps.sh --with-tauri`) and
builds its frontend: React/Solid/Svelte/Vue variants require Node.js plus
`pnpm`; Leptos/Yew variants require `trunk`, Tauri CLI 2, and the
`wasm32-unknown-unknown` Rust target. The helper checks these prerequisites up
front and prints the exact install command for anything missing.

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

## Remote control between JWM X11 sessions

Generate and securely copy one private key, explicitly expose the host on the
trusted LAN, then connect from the other JWM machine:

```bash
# Managed release bundle only; source installs already use /usr/local/bin.
export PATH="/usr/local/lib/jwm/current/bin:$PATH"

jwm-remote keygen --output ~/.config/jwm/remote.key
jwm-remote host --listen 0.0.0.0:48221 --allow-lan --allow-input \
  --key-file ~/.config/jwm/remote.key
jwm-remote connect 192.168.1.50:48221 --grab-input \
  --key-file ~/.config/jwm/remote.key
```

Direct LAN traffic is authenticated but not encrypted; the safer option is the
default loopback listener carried through SSH. See the complete setup,
security boundary, tuning, and current limits in
[JWM remote control](docs/remote-control.md).

The default modifier is Alt (`Mod1`). Useful built-in bindings include:

| Binding | Action |
| --- | --- |
| Alt+Shift+Return | Launch terminal |
| Alt+R | Application launcher (type `/` for open windows) |
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
| Alt+Shift+F9 | Cycle the WaterLily render palette |
| Alt+Shift+/ | Show all bindings |

During interactive screenshot or recording selection, press `G`, `W`, `M`, or
`D` to choose a dragged region, a window, the monitor under the pointer, or the
entire desktop. `Tab` and `Shift+Tab` cycle the same choices. Window capture
shows a hover preview and is confirmed with the left mouse button; `Enter`
saves a screenshot or starts/commits recording. Arrow keys nudge a committed
selection (`Shift` uses 10-pixel steps), while `Escape`, right-click, or the
recording shortcut again cancels safely.

### The screenshot editor

Once a screenshot region is committed the selection becomes an editor, and a
toolbar floats just outside it — below the selection, or above when there is no
room below. Every tool has both a button and a key, so neither the mouse nor
the keyboard is required:

| Tool | Key | Draws |
|---|---|---|
| Pencil | `P` / `F` | Freehand stroke |
| Line | `L` | Straight line |
| Arrow | `A` | Line with a head |
| Rectangle | `R` | Hollow rectangle |
| Filled rectangle | `B` | Solid redaction bar |
| Ellipse | `C` / `O` | Hollow ellipse |
| Marker | `H` | Translucent highlighter |
| Text | `T` | Typed label — click, then type |
| Counter | `N` | Auto-numbered bubble; click to place the next one |
| Pixelate | `X` | Drag a region down to blocks |
| Invert | `I` | Invert a region's colors |

The rest of the strip is the selection's pixel size, the stroke controls, the
ink swatch, and the four ways out:

| Control | Key |
|---|---|
| Thinner / thicker stroke | `-` / `+`, `Ctrl+Down` / `Ctrl+Up`, or the wheel |
| Ink (8-colour ring) | `1`…`8`, or click the swatch to step |
| Undo / redo | `Ctrl+Z`, `Backspace` / `Ctrl+Shift+Z`, `Ctrl+Y` |
| Save to file | `Enter` or `Ctrl+S` |
| Copy to clipboard | `Ctrl+C` |
| Cancel | `Escape` |

While a text label is open every key is text; `Enter` commits it, `Escape`
drops it, and switching tools commits it. A control with nothing to do — undo
with an empty history, thinner at the minimum width — is dimmed rather than
removed, so the row never reflows under the pointer.

The status bars' screenshot pill drives exactly this editor over the control
socket (the `take_screenshot` IPC command) rather than launching an external
grabber, so it works identically under X11 and Wayland and needs nothing
installed. `take_screenshot_fullscreen` captures the whole desktop with no
interaction.

The X11 compositor freezes the desktop behind the interactive selector/editor
by default. To keep clients and animations live instead, set this in the
`[behavior]` section; config reload and `set_config` take effect immediately:

```toml
[behavior]
screenshot_freeze_enabled = false
```

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

The built-in shell — the [control center](docs/control-center.md), native
[notifications](docs/notifications.md) with a freedesktop D-Bus service,
[media controls](docs/media-controls.md) driven from MPRIS, the
[calendar](docs/calendar.md), the [application launcher](docs/launcher.md),
the [wallpaper picker and its colour theming](docs/wallpaper.md),
[clipboard history](docs/clipboard.md), the [idle policy](docs/idle.md),
the [resource rows](docs/resources.md), and the
[session menu and night light](docs/session-menu.md) — is documented per
feature. Every status bar in `bars/` carries an entry that opens the
same [Shell Hub](docs/control-center.md#opening-the-shell-from-a-status-bar),
so a pointer-driven session reaches it without `Alt+F10`. Every panel key is a
toggle: `Alt+F10`, `Alt+F11`, `Alt+F12`, `Alt+F9` and the rest each take their
own surface back down, so nobody has to reach for `Esc`.
The Alt+Ctrl+Tab window switcher and the `cube` tag-switch transition are both
the Compiz-style lit prism documented in [cube effects](docs/cube-effects.md).
Alt+Space cycles layouts over a film strip of live thumbnails; see
[the layout picker](docs/layout-picker.md).
Minimized windows fold into the bar, expose a magnifying Dock shelf and show a
compositor-owned hover preview; the cross-process lifecycle is documented in
[the minimized-window Dock](docs/minimized-dock.md).
The compositor's live counters are on `Alt+Shift+F12`; see
[the debug HUD](docs/debug-hud.md).
Additional operational tools are documented in [tools/README.md](tools/README.md).
The external Julia simulation worker and frame protocol are documented in
[docs/waterlily.md](docs/waterlily.md).
Architecture boundaries and the incremental migration plan are in
[docs/architecture.md](docs/architecture.md). The delivery sequence for larger
changes is tracked in [the evolution roadmap](docs/roadmap.md).

## Releases and versioning

Release automation accepts only `jwm-v<semver>` tags whose version exactly
matches the root `jwm` package. It builds an x86_64 Linux bundle on Ubuntu 22.04,
a tagged source archive, `SHA256SUMS`, and artifact provenance. Other systems
should build from source; no multi-architecture binary support is claimed.

Monorepo components use independent SemVer. A JWM tag does not change the
version of the bridge, portal, shared protocol, bar core/providers, or bars.
See [CHANGELOG.md](CHANGELOG.md) and the config, IPC, session, backend, and
driver policies in [compatibility](docs/compatibility.md).

## Contributing and security

Before opening a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md). Please
report security-sensitive problems through the private process described in
[SECURITY.md](SECURITY.md), not a public issue.

## License

jwm is distributed under the [MIT License](LICENSE). The `portal/` crate
(the xdg-desktop-portal ScreenCast backend) is covered by the same license,
as already declared in its manifest.
