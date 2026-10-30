# Shared desktop shell

JWM supervises one `jwm-shell` process per session. Each output has a left
application/workspace panel, right sound/display panel, and bottom media/window
Dock. The selected status bar keeps its own toolkit and renders the primary
bar. The built-in and source-installer default is `tauri_leptos_bar`.

Implementation and acceptance are tracked separately. The authoritative frontend
inventory is [shared-shell-coverage.json](shared-shell-coverage.json); `pending`
means no current-build runtime acceptance exists. Historical binaries, compile
checks and mock browser tests do not prove native frontend integration.

## Configuration and rollback

Merge into the existing JWM configuration for the active backend:

```toml
[status_bar]
name = "tauri_leptos_bar"

[desktop_shell]
enabled = true
left = "hover"
right = "hover"
bottom = "hover"
reveal_delay_ms = 80
hide_delay_ms = 400
```

The new table is opt-in. Its explicit settings take precedence over legacy
`status_bar.layout = "four-edge"` and the old Tao `[edge_shell]` configuration.
Coordinated mode places the primary bar at the top and retains saved single-edge
positions for rollback. Set `[desktop_shell] enabled = false` to disable the
shared host. Installers preserve existing user configuration; `--bar NAME`
explicitly changes the selected frontend and backs up modified configuration.

New shared-shell configurations auto-hide all three panels. **Super+Shift+B**
toggles every enabled edge between auto-hide and pinned mode and saves the
configuration without restarting JWM. Disabled edges stay disabled. A mixed
configuration first pins all enabled edges; the next press auto-hides them.
Existing explicit modes and legacy four-edge pinned layouts remain respected.
The IPC equivalent is `jwm-tool msg toggle_desktop_shell_mode --raw`.

The default Glass and GlassDark menus share the edge panels' cool glass palette,
blue accent and ink hierarchy. Text-heavy menus use a denser veil for contrast;
the edge controls retain their lighter backdrop. Selected menu rows keep a
translucent blue wash so their text remains readable. Edge controls distinguish
hover and press without moving their click targets, and sliders keep a visible
handle even at zero.

Managed left/right/bottom panels follow the live `appearance.ui_theme` light/dark
choice, including Theme menu changes, on the existing refresh cycle (within
about one second). They no longer override it with the standalone `XBAR_CONFIG`
theme. The selected primary bar retains its own toolkit/theme configuration.
Alternate compositor themes retain their configured accent colors.

Top bars share the `tao_glow_bar` header profile: launcher, numbered workspaces,
centered client title and compact status indicators. Each monitor displays the
union of its occupied tags and its active tags, in their original numeric order.
An empty active tag remains visible. For example, occupied tags 1, 3 and 8 with
active tag 5 display **1, 3, 5, 8** and highlight 5. Minimized clients count toward
occupancy; another monitor's clients do not. Right-click toggles a workspace in
the current view, and the wheel visits the visible workspace sequence.

For an older running JWM that already supports shared-shell configuration,
`jwm-toggle-sidebar-mode` uses its existing IPC commands and persists the same
mode change. This compatibility helper requires Python 3.11+, or Python 3 with
`tomli` (Debian/Ubuntu: `python3-tomli`). It never restarts the desktop session.

The host's GUI dependencies have an independent Cargo boundary at
`crates/jwm_shell`. They do not enter JWM or the GUI-independent shared core.
Legacy Tao entry points reuse the extracted implementation, and a JWM-managed
Tao top bar cannot create private companions while the shared host owns them.

The right brightness slider controls panel backlights, never keyboard or network
LEDs. Debian/Ubuntu bootstrap installs `brightnessctl` and `brightness-udev`.
When the tool is absent, the host can read `/sys/class/backlight`; when a write
is denied, it uses `busctl` and the current logind session's
[`SetBrightness`](https://www.freedesktop.org/software/systemd/man/252/org.freedesktop.login1.html)
method. This works in an active local systemd session without adding the user
to the `video` group or running the host as root. Systems without logind still
need ordinary backlight write permissions. External displays without a kernel
backlight device remain unavailable. Check the detected hardware with
`jwm-shell --inspect-providers` and slider targets with `--inspect-controls`.

For an explicit legacy rollback on X11, disable `[desktop_shell]` and launch
JWM with `XBAR_LEGACY_PRIVATE_SHELL=1` plus `status_bar.name = "tao_glow_bar"`.
This opt-in re-enables the original independent Tao companion adapter only when
the new shared host is disabled. It cannot override active shared ownership.

## Protocol and ownership

Protocol version 1 uses an independent Unix IPC connection, never the primary
bar's shared message queue. `get_desktop_shell` supplies a full versioned snapshot
with session identity, monotonic revision, per-output geometry/visibility and
model state. Host registration binds its PID, session and panel roles. A reconnect
starts from a new full snapshot. Replaceable state is coalesced; commands have
bounded frames and deadlines. Stale sessions and window projection identities
must be rejected before actions alter JWM state.

The supervisor owns retries, health checks and cleanup. Changing or crashing the
primary bar must leave the shared host PID unchanged. Host failure must release
its reservations and recover with backoff. JWM visibility, fullscreen, lock and
output topology govern every panel; a mapped overlay alone must never reserve
workspace. Pinned reservations count external struts only once.

## Acceptance gates

| Stage | Required evidence |
|---|---|
| P0 | Contract, 28 frontend inventory, clean-baseline captures and measurements |
| P1 | Shared rendering/geometry/actions; legacy native regressions and deterministic tests |
| P2 | Independent host, bounded state channel/reconnect and process shutdown |
| P3 | Tao pilots, stable host across frontend replacement, crash and workspace recovery |
| P4 | Current build and actual process/interaction for each of 28 frontends on XCB and X11RB |
| P5 | Source install, bundle install/upgrade/rollback, diagnostics, CI and archived logs/captures |
| P6 | Native Wayland panel surfaces/input/exclusive zones and a separately recorded hardware matrix |

XWayland results must not be reported as native Wayland validation. Screenshots
must come from JWM's compositor. Missing hardware or toolkit dependencies remain
explicit unverified gates until exercised on the required environment.

Run the parameterized native fixture after building current JWM, host and bar:

```sh
python3 scripts/test-shared-sidebar-shell-x11.py --bar tao_pixels_bar --backend xcb
python3 scripts/test-shared-sidebar-shell-x11.py --bar tauri_leptos_bar --backend x11rb
```

Add `--submenu-themes --theme light` (or `dark`) to capture the Hub, Theme,
Applications, Notifications and Calendar menus, exercise native back navigation,
and verify a persisted Theme-menu change updates the managed edge snapshot
without replacing its host. The fixture changes only its private configuration.
Menu captures require ImageMagick `import` and read the final composited X11
display: ordinary compositor IPC screenshots deliberately exclude modal chrome.
The evidence records each capture's source.
Install the configured UI and icon fonts for visual acceptance. On a test host
without the configured family, `--ui-font 'DejaVu Sans 11'` selects an installed
TrueType family in the private fixture only; symbol fallback still needs an
installed Font Awesome or Nerd Font.

The fixture uses a private Xvfb, Unix socket and configuration. Evidence includes
actual process IDs, registered output/roles, compositor screenshots, test steps
and failure reasons. The full matrix must retain individual rows rather than
folding experimental toolkits into a blanket pass.
