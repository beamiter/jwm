# Four-edge bars

## Frosted four-edge desktop

The official `tao_glow_bar` now combines a rounded information bar at the top
with three independent floating panels: launchers and workspaces on the left,
sound/display and Shell Hub shortcuts on the right, and media plus a live task
dock at the bottom. The shared light palette uses ice-blue glass, soft white
controls and a blue active workspace. Dark themes and explicit font/label
overrides remain supported. Symbols are drawn as vectors or use a portable
text fallback when a Nerd Font is unavailable.

This independent companion workflow uses JWM's `status_bar.layout = "single"`,
the default. The primary bar keeps its configured position, while xbar's
`[edge_shell]` controls each additional panel. The separate
[coordinated four-panel mode](#four-panels-at-once) pins the whole group and
reserves all four workspace edges on native X11.

Keep JWM's `[status_bar] name = "tao_glow_bar"`, `position = "top"` and
`layout = "single"` for the hover desktop. Merge this into
`~/.config/xbar/config.toml`:

```toml
theme = "light"
font = "Sans 11"
background_opacity = 0.58

[edge_shell]
enabled = true
left = "hover"
right = "hover"
bottom = "hover"
reveal_delay_ms = 80
hide_delay_ms = 400
trigger_pixels = 2
```

For the reference image's larger header proportions, merge this into the
active JWM `config_x11.toml` or `config_wayland.toml`:

```toml
[appearance]
status_bar_height = 52
status_bar_padding = 12
```

Existing JWM thickness/margin settings remain authoritative. The installer
also preserves an existing `status_bar.name`; upgrade an older pixels-bar
installation explicitly with `scripts/install_jwm_scripts.sh -b tao_glow_bar`.

Each edge accepts `hover`, `always` or `disabled`. For example, changing only
`bottom = "always"` pins the dock while the side panels continue to auto-hide.
The running companions reload per-edge modes and hover timing after a file
edit, within the next status update (normally one second). Invalid edits keep
the last valid modes. Changing the global `enabled` switch, fonts or the header
style still requires restarting the bar. `enabled = false` turns off the
companions after restart when JWM uses `layout = "single"`.
Existing single-edge placements remain available;
companions are omitted when the main bar is placed on another edge.

Invisible sensors occupy two physical pixels along their entire
output edge, apart from the shared corner pixels. They are independent of the
centered capsule's size: approaching the far end of a large display works too.
Entering one sensor reveals only its panel. Moving between the edge strip and
its floating capsule retains the same hover ownership; leaving both schedules
a hide which re-entry cancels. Temporary pointer grabs do not count as physical
exits. While a hover panel is open, its actual pointer window is checked every
50 ms, so moving out during another client's grab still allows it to hide;
rootless XWayland's stale root coordinates do not keep it open over a native
Wayland client. Dragging a slider holds the panel open. Pinned panels
publish EWMH reservations at outer desktop edges; auto-hidden panels overlay
the workspace without changing its layout. Panels follow their monitor and
hide when JWM hides the bar, a fullscreen window owns the output, or the
monitor is locked. Short IPC timeouts retain the last observed visibility,
rather than causing pinned panels to blink off.
X11 uses input-only trigger windows. Rootless XWayland instead receives
transparent mapped trigger surfaces, so the Wayland compositor can route
pointer events to them. Managed bars inherit the active session's XWayland
`DISPLAY`, including when JWM's own inherited host display is different.

The task dock obtains the real window inventory from JWM's bounded Unix IPC,
including native Wayland clients. Clicking a task reveals and focuses it;
`+N` cycles through overflow tasks. Available audio/brightness sliders accept
click, drag and wheel input. Missing hardware displays an unavailable reading.
The launchers use `xdg-open`, `x-terminal-emulator` and `x-www-browser`; a
missing executable opens JWM's application launcher instead.

The companion surfaces require X11 or XWayland, Cairo/Pango and GTK3 (the same
toolchain as the official bar). A compositor provides native translucency;
without one, the bar samples and frosts the wallpaper. As with the existing
glass fallback, this samples the wallpaper rather than live application
content. For an exact sampled background, set `[glass] wallpaper` and
`wallpaper_mode` to match JWM. This companion host belongs to the official
bar; the toolkit examples retain their individual single-edge window hosts
and inherit the shared light palette.

Build and exercise the real interaction regression in an isolated display:

```sh
cargo build --locked --bins
cargo build --locked --manifest-path bars/tao_glow_bar/Cargo.toml
dbus-run-session -- python3 scripts/test-edge-shell-x11.py
```

The test starts and stops its own Xvfb, uses temporary configuration, exercises
all three reveal modes (including changes without restarting), checks edge
triggers above later application windows, switches a live JWM workspace and
activates/restores a task. Slow transfers through the edge strip, transparent
passage and panel are checked on all three edges with 0/40 ms hide delays.
Window unmap events are observed to detect brief flicker, including stationary
external grabs, moving out during a grab, and button drags released outside.
Set `JWM_EDGE_SCREENSHOT=/absolute/path/desktop.png` to capture the test desktop
when ImageMagick's `import` is installed.
For a like-sized reference capture, set `JWM_EDGE_TEST_SIZE=1672x941` and
`JWM_EDGE_TEST_REFERENCE=1`; the latter applies the 52/12 geometry only to the
test's temporary JWM configuration.

### One bar without companion panels

Use `layout = "single"` in JWM's existing `[status_bar]` table, and disable
the companions in `~/.config/xbar/config.toml`:

```toml
[edge_shell]
enabled = false
```

Restart the bar after changing `enabled`. The primary bar can still use any
of the four single-edge placements. Disabling `[edge_shell]` alone does not
override JWM's explicit coordinated `layout = "four-edge"` mode.

## Four panels at once

The native X11 `tao_glow_bar` frontend can display a coordinated shell with
four panels on every output: a top status bar, left application/workspace
rail, right audio/display controls, and a bottom media/minimized-window Dock.
Enable it in the active X11 configuration:

```toml
[appearance]
status_bar_height = 42
status_bar_padding = 5

[status_bar]
name = "tao_glow_bar"
show_bar = true
layout = "four-edge"
```

Existing configurations keep `layout = "single"`: the primary bar uses its
configured `position` and xbar may still create independent hover/pinned
companions as described above. `layout = "four-edge"` selects a coordinated
host instead. Its four surfaces stay visible together, and the mode takes
precedence over xbar's `[edge_shell]` reveal settings; it does not start a
second set of companion panels. The top panel owns the managed bar window,
and its process creates the other three surfaces. The saved global
and per-output `position` preferences are retained and take effect again when
switching back to `single`. The four-panel mode currently requires the native
`x11rb` or `xcb` backend; configuration validation rejects it for Wayland or
for a different frontend.

At 1280×800 with the settings above, the panels occupy these physical rectangles:

| Panel | x | y | Width | Height |
|---|---:|---:|---:|---:|
| Top | 5 | 5 | 1270 | 42 |
| Left | 12 | 72 | 60 | 656 |
| Right | 1208 | 72 | 60 | 656 |
| Bottom | 128 | 712 | 1024 | 76 |

`FourEdgeGeometry` is shared by JWM and the frontend. It scales the rails and
Dock with the configured physical top-bar thickness, bounds tiny outputs,
and preserves negative output origins. The workspace reserves all four sides,
so tiled, maximized and newly placed windows avoid the complete shell. At the
size above it reserves 84 px left/right, 52 px above and 100 px below.
Existing external struts count toward those depths rather than being charged
twice. Fullscreen and the per-tag bar visibility preference apply to the whole
group.

For the reference's light glass, merge `theme = "light"` into
`~/.config/xbar/config.toml`, or select the supplied
[`four-edge.toml`](../bars/tao_glow_bar/four-edge.toml) with `XBAR_CONFIG` when
launching JWM. The bottom Dock uses window-title buttons; hovering a button
still opens the compositor's live preview. Unavailable battery or brightness
readings remain `--`, and an unavailable slider has no adjustment target.
JWM publishes a wallpaper-only root pixmap for this mode even with its GPU
compositor enabled, so each panel samples the correct per-output/tag wallpaper
without a second path in the bar configuration. Procedural wallpapers use the
configured static fallback for this sampled material.

Changing `status_bar.layout`, thickness, shell padding or the output rectangle
restarts that output's bar process and all its surfaces together. JWM supplies
`JWM_BAR_LAYOUT`, `JWM_BAR_PADDING`, and `JWM_BAR_OUTPUT_X`, `_Y`, `_WIDTH`,
`_HEIGHT` alongside the existing managed-bar environment. These values are
physical output pixels. `jwm-tool msg get_show_bar` reports `bar_layout`, and
`get_config` reports `status_bar_layout`.

To switch temporarily without editing the file:

```sh
jwm-tool msg set_config --args '{"key":"status_bar.layout","value":"four-edge"}'
jwm-tool msg set_config --args '{"key":"status_bar.layout","value":"single"}'
```

After building JWM and `bars/tao_glow_bar`, exercise the real native windows,
pointer actions, fullscreen behavior, process replacement and work areas in
private display sessions:

```sh
JWM_TEST_BACKEND=xcb python3 scripts/test-four-edge-shell-x11.py
JWM_TEST_BACKEND=x11rb python3 scripts/test-four-edge-shell-x11.py
```

`JWM_FOUR_EDGE_SCREENSHOT=/absolute/path/shell.png` additionally saves a PNG
from JWM's compositor. A root-window `xwd` readback omits the composited overlay
and cannot verify the alpha corners of these panels.

## Single-edge placement

JWM can place each output's primary bar at the top, bottom, left or right.
The window manager owns the physical bar rectangle and the workspace it
reserves; frontends lay out readable controls inside that rectangle. Sidebars
keep text, icons and minimized-window cards upright. These placement settings
apply to `layout = "single"`; coordinated mode keeps its primary surface at
the top and retains the single-edge preference for a later switch back.

`tao_glow_bar` remains the supported daily-drive bar installed by
`scripts/install_jwm_scripts.sh`. The other bundled bars are examples for
different toolkits and rendering backends. They share the placement contract,
but retain their own native widget behavior and rendering capabilities. See
[Status bars](../bars/README.md) for installation and
[Shell Hub](control-center.md#bar-placement-and-size) for the native controls.

## Configure each output

Edit the active JWM configuration:

- X11: `~/.config/jwm/config_x11.toml`.
- Wayland: `~/.config/jwm/config_wayland.toml`.

These paths follow the system configuration directory when `XDG_CONFIG_HOME`
is set. Merge the following keys into existing tables:

```toml
[appearance]
status_bar_height = 48
status_bar_padding = 4

[status_bar]
name = "tao_glow_bar"
show_bar = true
position = "top"

[[status_bar.monitors]]
monitor = 0
position = "left"

[[status_bar.monitors]]
monitor = 1
position = "bottom"
```

`position` accepts `"top"`, `"bottom"`, `"left"` and `"right"`. A monitor entry
overrides the global position for that output. Omitted positions in old
configurations default to top; monitor entries themselves require a valid
position. Monitor numbers are the IDs reported by `jwm-tool msg get_monitors`,
and a disconnected monitor's ID can later be reused.

Global position changes preserve explicit monitor overrides. To return every
monitor to the global default, remove the `[[status_bar.monitors]]` entries or
set `monitors = []` inside `[status_bar]` after removing those entries.

### Units and precedence

| Setting | Meaning | Unit and default |
|---|---|---|
| JWM `status_bar.position` | Default edge for outputs without an override | `top` |
| JWM `status_bar.monitors` | Per-monitor edge overrides | Empty list |
| JWM `appearance.status_bar_height` | Cross-axis thickness, including sidebar width | Physical pixels; 42 by default |
| JWM `appearance.status_bar_padding` | Margin outside the bar and between the bar and workspace | Physical pixels; 5 by default |
| Bar `[presentation] position` | Standalone edge preference | `top` |
| Bar `[presentation] bar_height` or `thickness` | Standalone cross-axis thickness | Logical pixels; 42 by default |
| `JWM_BAR_POSITION` | Effective edge supplied to a managed bar | Overrides `XBAR_POSITION` and the bar file |
| `JWM_BAR_THICKNESS` | Effective physical thickness supplied to a managed bar | Overrides standalone logical thickness |
| `XBAR_POSITION` | Optional standalone process override | Overrides the bar file when `JWM_BAR_POSITION` is absent |

The historical names `status_bar_height` and `bar_height` are retained for
configuration compatibility. Both mean height for horizontal bars and width
for vertical bars. `thickness` is a TOML alias for the bar's logical
`bar_height`; configure one of the two keys, not both.

For example, a managed bar with `status_bar_height = 48` occupies 48 physical
pixels at both 1× and 2×. At 2×, its frontend receives a 24-logical-pixel
cross-axis allocation. A standalone `thickness = 48.0` instead requests 48
logical pixels, or 96 physical pixels at 2×. This conversion keeps the managed
bar and the compositor's reserved strip in agreement.

Standalone appearance lives in `~/.config/xbar/config.toml`:

```toml
[presentation]
position = "right"
thickness = 42.0
shell_routes = ["hub", "notifications"]

[presentation.visibility]
monitor = false
```

`XBAR_CONFIG` can select a different file. Shared bar configuration is loaded
at startup, so restart a standalone bar after editing it. Managed edge and size
come from JWM even when the bar file requests another value. Invalid edge
strings, nonpositive physical thickness and duplicate TOML aliases are rejected
instead of silently choosing an unrelated placement.

### Change placement from the desktop

The Shell Hub's **Sound & Display** section has three persistent controls:

| Control | Scope | Interaction |
|---|---|---|
| **Bar edge · screen N** | Selected monitor | Left/Right, wheel, or Enter/click cycles top → right → bottom → left |
| **Bar size · all screens** | All monitor bars | Adjusts thickness in 2-physical-pixel steps, within 32–128 px in the UI |
| **Bar margin · all screens** | All monitor bars | Adjusts padding in 2-physical-pixel steps, within 0–32 px in the UI |

The Hub saves the selected setting to the active JWM file while preserving
unrelated settings and comments. A failed save leaves the previous live value
and produces a notification. Edge and thickness changes briefly restart the
affected bar processes; padding changes reposition existing bars. Workspaces,
fullscreen state and per-tag visibility remain owned by JWM during this update.

For temporary live changes from scripts:

```sh
jwm-tool msg set_config --args '{"key":"status_bar.position","value":"bottom"}'
jwm-tool msg set_config --args '{"key":"status_bar.monitors","value":[{"monitor":0,"position":"left"}]}'
jwm-tool msg set_config --args '{"key":"appearance.status_bar_height","value":48}'
jwm-tool msg set_config --args '{"key":"appearance.status_bar_padding","value":0}'
```

`set_config` updates live state until the next file reload; the Hub controls
persist their edits. `get_config` reports the global position and monitor
overrides. `get_show_bar` also reports the focused output's effective
`bar_position`, `bar_thickness` and `bar_padding`.

## Interaction and overflow

### Shared Scene bars

The supported Tao/Glow bar and the other Scene consumers use one
`PresentationProjector` and `LayoutEngine`:

- Leading workspaces and layout controls follow the long axis. Trailing status
  controls use the opposite end. The focused title uses remaining horizontal
  space; vertical bars can show its application icon without turning the title
  into a tall strip of letters.
- Clock components stack as upright `HH`, `MM` and optional `SS`. Short metric
  values stack beneath their icons. Native tooltips and accessible labels can
  retain the complete semantic description.
- Workspace allocation first tries full labels, then every workspace at its
  compact width, and only then hides workspaces behind overflow. Names that
  cannot remain distinct at the compact font floor use their workspace number;
  the semantic control retains its complete configured name.
- The WM's actual active workspace stays visible when the full strip cannot
  fit, including when earlier workspaces are also selected. Urgent, selected
  and occupied workspaces receive priority. Hidden urgent workspaces mark the
  overflow `!+N` as well as using the urgent color. `+N` advances to the
  next workspace; right-click goes to the previous one. The wheel over any
  workspace or overflow control traverses the full workspace list.
- The layout button opens available choices. Right-click or wheel cycles
  layouts when the expanded choices cannot fit. Space pressure never requires
  a tiny version of every layout option.
- Status allocation prioritizes the clock, critical battery and frequently
  used controls. Compact readings precede optional detail. A status `…` opens
  the Shell Hub, and its secondary action opens audio controls.
- Controls reserve at least 32 logical pixels along the long axis when the
  viewport permits. Hit targets extend through the bar's short axis without
  enlarging the painted pills. With JWM margin set to zero, these targets reach
  the physical screen edge. Extremely small viewports are clipped to their
  actual bounds and cannot provide that minimum for every control.

Pointer actions preserve their established meanings: workspace primary selects
and secondary toggles; volume and brightness respond to the wheel; the clock
opens Calendar and right-click toggles seconds. Press and release must resolve
to the same control and captured action, so a model update cannot substitute a
new workspace-overflow action or a new incarnation of a minimized window.

Tao/Winit presenters share precision-scroll accumulation with logical-DPI
thresholds and gesture boundaries. A mouse notch remains immediate; a fine
touchpad gesture commits navigation once, while volume/brightness can keep
adjusting. See [wheel and touchpad input](../bars/README.md#wheel-and-touchpad-input)
for thresholds, axis behavior and the exact frontend scope.

### Stable telemetry slots

Shared Scene bars reserve independent icon and value columns for CPU, memory,
volume, brightness and battery percentages. The value column holds the whole
`0%` through `100%` range and the unknown reading `--`, and aligns numbers to
the right. Column widths come from the configured font's measured glyphs,
including every mute, charging and unavailable icon candidate. Proportional
fonts therefore do not need padding spaces or a hard-coded pixel width.
Network receive/transmit columns retain their existing independent slots.

The same fixed dimensions participate in compact/full allocation, so changes
in CPU or memory usage do not make neighboring controls toggle between full
and compact modes near a width boundary. Sidebars reserve a stable icon row
and fit their numeric font size against the complete percentage range. This
also keeps `100%` readable in a 42-physical-pixel bar at 2x scaling without
changing the value's size when the next reading is `9%` or `--`. Viewport,
font, configuration and visible-control changes may still change the layout;
critical battery priority is retained when space is insufficient.

GTK/Relm use separately constrained native labels for icon and value, capping
both minimum and natural requests. The shared HTML adapter gives percentage
pills fixed grid columns with tabular numerals. Hover highlights retain their
one-pixel border and use an inset shadow, avoiding the previous border-width
change. No additional configuration is required for these paths. GPUI,
GPUI-component, Iced and Xilem retain their experimental widget renderers.

Pango measurement memoization shares repeated glyph probes within one
font/context snapshot. The normal Cairo presenter recreates that snapshot
each frame, keeping DPI and font changes current; retained standalone
measurers cap the cache at 512 short strings across font sizes.

Run the actual Pango/Cairo allocation and pixel regressions with:

```sh
cargo test --locked -p xbar_core --features render-cairo --test metric_columns
```

Set `XBAR_METRIC_PREVIEW=/tmp/jwm-metric-preview.png` to also save a six-row
CPU/memory comparison for `0%`, `9%`, `10%`, `99%`, `100%` and `--`.

### Widget and HTML bars

GTK/Relm and retained toolkits adapt their groups to the selected axis. GTK
controls expose full native tooltips, focusable buttons and a visible focus
outline; overflowing groups scroll into view through native focus navigation.
Toolkits keep their own focus handling. The shared Scene itself remains a
pointer presentation; its host and JWM's normal workspace/layout keybindings
provide keyboard commands. The experimental Iced adapter currently has no
explicit keyboard traversal for its bar controls. Xilem tooltips are drawn
inside the bar window and can be clipped by a narrow sidebar; JWM's native
minimized-window previews use the inward-facing placement for the selected
edge.

The HTML adapter shared by Dioxus and all six Tauri applications installs:

- One Tab entry for the toolbar, visible focus indication, and Enter/Space
  activation for legacy controls. Tab/Shift+Tab can leave the toolbar instead
  of visiting every workspace and status pill. Initial entry prefers the
  actual active workspace; re-entry remembers the previous control.
- Left/Right navigation for horizontal bars and Up/Down for sidebars, following
  displayed control order. Home/End selects the first/last available control.
- Automatic scrolling of focused controls into view, with overflow-edge cues
  that reserve no layout space. Disabled controls are skipped and do not
  activate. A removed or replaced focused control recovers to its equivalent
  or the nearest remaining control while the bar still owns document focus.
- PageUp/PageDown or plus/minus adjusts a focused volume/brightness control;
  Escape closes the layout choices. Modifier gestures and horizontal movement
  cannot accidentally enter a metric's vertical wheel handler. Fractional
  pixel scrolling accumulates before changing volume/brightness, and a group
  scroll keeps ownership when a status control moves beneath the pointer.
- Explicit selected/current workspace semantics remain available when urgent
  styling takes priority. Icon glyphs are decorative for accessibility; status
  readings and mute/expanded states have semantic labels.
- Upright compact clock text, semantic ARIA labels and bounded scrollable
  workspace/status groups. The clock and frequently used controls begin the
  sidebar status group so they remain initially visible.

The adapter preserves each frontend's action dispatch. An example frontend
can retain its existing primary/secondary behavior rather than adopting every
Scene binding. Popovers and compositor-owned Dock previews open toward the
usable output interior where their host supports them.

### Default OpenGL rendering

`tao_glow_bar` retains a reusable Cairo CPU canvas and a GL texture. A size
change allocates the texture. Without an external backdrop, later frames
upload only the shared damage rectangle, including padded source rows, and
identical frames skip uploading.
Native expose events still repaint the window's back buffer. WM, pointer and
resize invalidations share one pending native redraw, so a burst of updates
paints the latest model without synchronously presenting intermediate states.
There is no continuously running frame timer.

The shared CPU canvas invalidates its complete frame on DPI changes and
conservatively uploads every pixel while an external backdrop is present,
including the first frame after its removal. Renderer/font/opacity changes
also invalidate pixels even when scene geometry stays equal. A failed GL
upload discards its retained-state acknowledgement so the next redraw replaces
the whole texture. The opaque X11 wallpaper source bounds root-pixmap lookup
passes to one per 200 ms, including lookups made by the render path.

The native pixel regression exercises the production uploader against full
Cairo frames on all four edges at 1x, 1.5x and 2x. It also checks padded rows,
partial updates, resize and recovery from invalidated texture storage:

```sh
xvfb-run -a cargo test --manifest-path bars/tao_glow_bar/Cargo.toml \
  retained_texture_matches_full_frames -- --ignored
```

This test needs X11/EGL and is deliberately ignored by a display-free unit
test invocation. It establishes pixel correctness, not GPU timing on a user's
physical desktop. The rendering changes here target the supported default GL
bar; experimental presenters retain their own presentation scheduling.

### Minimized-window Dock

Window thumbnails remain landscape cards at all four edges. Magnification
follows the pointer's x coordinate on horizontal bars and y coordinate on
sidebars, and is bounded by available thickness. Neighbor magnification uses
reserved space; a pointer never has to chase a moving interaction target.

Shared Scene and `DockBridge` overflow controls expose the newest locally
omitted window through `+N`. Restoring it makes room for remaining windows on
the next authoritative snapshot. A protocol-level overflow flag with no
corresponding token remains `…`; the frontend cannot invent an action for an
unknown window. See [Minimized Dock](minimized-dock.md) for the compositor's
preview, restore and session rules.

## Geometry and architecture

### One physical rectangle per managed bar

JWM derives the bar rectangle and workspace reservation together in
[`BarGeometry`](../src/jwm/bar_geometry.rs). For a normally sized output with
origin `(x, y)`, size `(W, H)`, thickness `t` and padding `p`:

| Edge | Physical bar rectangle `(x, y, width, height)` | Workspace edge reserved |
|---|---|---|
| Top | `(x+p, y+p, W−2p, t)` | Top, depth `t+2p` |
| Bottom | `(x+p, y+H−p−t, W−2p, t)` | Bottom, depth `t+2p` |
| Left | `(x+p, y+p, t, H−2p)` | Left, depth `t+2p` |
| Right | `(x+W−p−t, y+p, t, H−2p)` | Right, depth `t+2p` |

Tiny outputs clamp thickness and padding to a positive representable surface
and leave at least one physical pixel for layout. Existing external struts are
discounted on the selected edge so the same area is not reserved twice.

Managed X11 bars use JWM's per-output reservation. EWMH partial struts describe
root-window edges, so an interior monitor edge advertises no global strip that
would consume an adjacent output's workspace. Outer-edge struts include padded
depth and inclusive ranges. Standalone native adapters use
`EwmhStrut::for_edge` with the real root geometry. The `LayerShellPlacement`
helper computes the requested anchor, perpendicular stretch anchors and logical
exclusive zone for hosts using layer shell. This refactor does not turn an
ordinary Tao/winit Wayland window into a standalone layer-shell client.

### Authoritative origin and scaling

The historical `MonitorGeometry` transport name requires care. In a managed
bar snapshot, `x/y` are **already the padded bar's physical desktop origin**,
including bottom/right placement. `width` is actual bar width. For sidebars,
`height` is available vertical length; horizontal snapshots retain the existing
output-height field and use authoritative thickness for the short axis.

Use `BarPlacement::from_bar_geometry` for this snapshot. Use
`BarPlacement::at_edge` only when the input is an actual output rectangle.
Applying the latter to a managed snapshot would shift bottom and right bars a
second time.

Scene and widget rectangles are logical and bar-local. Conversion to compositor
targets is:

```text
physical_x = bar_origin_x + round(local_x * scale)
physical_y = bar_origin_y + round(local_y * scale)
physical_width = round(local_width * scale)
physical_height = round(local_height * scale)
```

Only local coordinates and extents are scaled. Desktop origin is already
physical, including negative positions on multi-monitor desktops.

### Shared responsibilities

| Layer | Responsibility |
|---|---|
| JWM configuration / `BarGeometry` | Per-output edge, physical rectangle, work area, managed process placement and restart |
| `BarEdge` / placement helpers | Four-edge native rectangles, EWMH properties and layer-shell anchors |
| `PresentationProjector` | Semantic state, compact/sidebar content, actions, accessibility names and tooltips |
| `LayoutEngine` / `Scene` | Axis-aware allocation, priority/overflow, upright primitives, clipping and stable hit regions |
| `DockMetrics` / `DockReporter` | Shared card/slot measurements, stable geometry, visual anchors, session validation and delivery |
| `DockBridge` | Exact retained-toolkit allocations and projection-scoped item/overflow bindings |
| `web_layout` | Common HTML axis CSS, keyboard focus and viewport/ancestor-clipped DOM geometry |

These policies live in `crates/xbar_core`; GUI dependencies remain at frontend
boundaries. Fifteen bars consume the shared Scene: egui, four Tao variants,
four winit variants, three x11rb variants and three XCB variants. GTK/Relm share
`xbar_gtk`; GPUI/GPUI-component, Iced, Xilem and Dioxus use their own widget or
DOM hosts. Six Tauri frontends share `xbar_tauri`. The GUI packages and adapters
have independent Cargo boundaries so their dependency graphs do not become
root-workspace requirements. `xbar_tauri` now explicitly declares `[workspace]`
and can be checked through its own manifest.

### Retained allocations must match reported geometry

After `DockBridge::set_edge` and `synchronize`, these accessors all return
**logical bar-local** rectangles:

| API | Native allocation |
|---|---|
| `shelf_bounds()` | Fixed shelf in the bar root |
| `slot_bounds(token)` | Stable whole-thickness hit/focus target |
| `resting_item_bounds(token)` | Unmagnified persistent compositor target |
| `item_bounds(token)` | Exact current visual card, including magnification |
| `overflow_bounds()` | Actual actionable overflow target |

Place the shelf outside workspace/status scrollers and reserve its main-axis
extent from those groups. Subtract the shelf origin when positioning a child
relative to it, or the slot origin when positioning a card within its target.
`item_bounds` already includes magnification. Applying a second scale,
independent padding or a native button's implicit minimum size would make the
visible card diverge from its compositor anchor.

Capture `item_binding(token)` or `overflow_binding()` when rendering an
interactive control. The binding contains WM session, minimized generation and
token. `request_restore(binding)` validates it and retains the bounded click
through transport backpressure/recovery. Callbacks must not rebuild an old
control's binding from the bridge's newest projection.

HTML frontends with their own flowing Dock instead measure the real DOM after
layout. `visibleDockRect` clips a current visual rectangle against the viewport
and every clipping/scroll ancestor. `restingDockRect` removes the card's known
translation/scale from its current viewport rectangle before clipping.
Scroll/resize changes trigger geometry publication, and hidden or detached
cards withdraw their targets. Adding `offsetLeft/offsetTop` to a synthetic
shelf origin would lose nested scrolling and is not a valid substitute.
Dioxus uses the fixed `DockBridge` allocation path and is tested to remain
pinned when neighboring status content scrolls.

## Validation

The four-edge refactor has been checked at these distinct levels:

| Check | Completed coverage |
|---|---|
| Shared kernel | 365 all-feature unit tests and 2 doctests, including four-edge scene/placement, DPI, clipping, overflow and Dock identity/recovery |
| JWM targeted suite | 359 tests covering configuration, bar geometry/work areas, related popup/preview behavior and persistence |
| GTK adapter | 11 tests |
| Tauri bridge | 13 library tests, including four-edge standalone placement and current-output/DPI recovery |
| Dioxus adapter | 6 tests, including current-output/DPI recovery and protocol-only Dock overflow |
| Shared core lint and format | All features/targets pass Clippy with warnings denied; core formatting passes |
| Rust package checks | 22 non-Tauri bar packages, including all 15 Scene consumers and GTK/Relm, Dioxus, GPUI/GPUI-component, Iced and Xilem; standalone Tauri bridge |
| JWM feature coverage | Library compiles with all features enabled |
| JavaScript application builds | React, Solid, Svelte and Vue production builds |
| Rust/Wasm frontend checks | Leptos and Yew compile for `wasm32-unknown-unknown` |
| Browser layout fixtures | 84 layout/accessibility cases: 7 frontend stylesheets × 4 edges × 3 sizes; additional transformed/scrolling geometry and 4 pinned Dioxus allocation checks |
| Production-bundle browser tests | 16 cases: actual built React/Solid/Svelte/Vue frontends × 4 edges in Chromium with a mock Tauri IPC host; readiness, single keyboard activation, visual-order navigation, clipped/scrolling Dock reports and keyboard restore geometry |
| Actual native X11 smoke | `tao_glow_bar` at all 4 edges; GTK at all 4 edges with 1× and 2× scaling, plus live 1×→2×→1× transitions for horizontal/vertical bars with stable physical size, struts and focused control |
| Actual Cairo/Pango rendering | 4 edges × 2 themes × 2 scales, using populated workspace/status/Dock scenes |

The layout regression loads real frontend stylesheets and the shared adapter
into controlled DOM fixtures. The production-bundle harness loads each built
JavaScript application and supplies an in-memory Tauri readiness/event/action
host. Both run in Chromium; native Tauri/WebKit runtime behavior still requires
desktop validation. Package checks do not establish visual parity across
experimental toolkits. Physical Wayland hardware, every multi-output topology,
and every toolkit's native popup styling remain outside this coverage. The
Leptos/Yew checks compile their Rust/Wasm targets; they do not exercise complete
Tauri webview applications.

Run the reproducible shared checks from the repository root:

```bash
cargo test -p xbar_core --all-features
cargo test -p jwm --no-default-features --lib --offline -- bar dynamic_island system_ui_panel
cargo check -p jwm --lib --all-features
cargo check --manifest-path bars/tao_glow_bar/Cargo.toml
cargo check --manifest-path crates/xbar_core/crates/xbar_tauri/Cargo.toml
cargo test --manifest-path crates/xbar_core/crates/xbar_tauri/Cargo.toml --lib
cargo check --manifest-path bars/tauri_leptos_bar/Cargo.toml --target wasm32-unknown-unknown
cargo check --manifest-path bars/tauri_yew_bar/Cargo.toml --target wasm32-unknown-unknown
node scripts/test_bar_layout.mjs
```

The browser scripts require Playwright and a Chromium installation.
`PLAYWRIGHT_MODULE` can point to an installed module, `CHROME_BIN` can select a
browser executable, and `BAR_SCREENSHOTS` can select a directory for the layout
script's visual fixtures. For the production-bundle suite, install each
frontend's Node dependencies, build the four applications, then run:

```bash
for frontend in react solid svelte vue; do
  npm --prefix "bars/tauri_${frontend}_bar" run build
done
node scripts/test_bar_web_frontends.mjs
```

The existing `native-bars` CI workflow runs the shared browser fixtures once
and the production-bundle checks in each locked JavaScript frontend job.
`BAR_FRONTENDS=react` (or a comma-separated subset) selects the built frontends
for `test_bar_web_frontends.mjs`. These jobs use Chromium with a mock IPC host;
native Tauri/WebKit behavior remains a separate desktop validation step.

### Reusable Cairo render fixture

```bash
cargo run -p xbar_core --features render-cairo --example render_four_edges -- /tmp/jwm-bar-previews
```

[`render_four_edges.rs`](../crates/xbar_core/examples/render_four_edges.rs)
creates sixteen PNGs and sixteen matching scene JSON files, named by edge,
theme and scale, such as `left-dark-2x.png` and `left-dark-2x.json`. It renders
the production scene with Pango measurement and Cairo, without a running WM,
display server or provider processes. The fixtures use portable labels so
missing Nerd Fonts do not obscure layout inspection. Install the configured
icon font when evaluating native screenshots that use private-use glyphs.

## Fixed-physical sidebar regression

Managed sidebars retain readable workspace labels and clock digits at 32,
42 and 48 physical pixels across 1x, 1.25x, 1.5x and 2x output scales.
Decorative outer and inner padding yield to the available logical width;
text stays upright and the pointer target still spans the full bar width.
This is different from a 42-logical-pixel standalone fixture, which grows
to 84 physical pixels at 2x and did not expose the blank-sidebar regression.

The production Cairo/Pango test checks both edges, both themes, normal and
oversized padding, complete clock/workspace labels, actual glyph pixels,
containment and full-width hit targets (96 configurations):

```sh
XBAR_SIDEBAR_PREVIEWS=/tmp/jwm-managed-sidebars \
  cargo test --locked -p xbar_core --all-features --test managed_sidebars
```

PNG and Scene JSON outputs are optional; the regression always runs under
the existing all-feature/all-target core test job without a display server.
