# JWM wayland-udev Competitive Roadmap

Last checked: 2026-07-06.

This note compares JWM's native `wayland-udev` backend with the two useful
reference points for a modern Wayland WM:

- niri: a Rust/Smithay compositor whose identity is scrollable tiling, stable
  multi-monitor behavior, dynamic workspaces, overview, gestures, screenshots,
  screencasting, tabs, blur, animations, live reload, accessibility, and broad
  input-device support.
- Hyprland: a high-energy Wayland compositor whose strength is the control
  surface and ecosystem: rich layouts, animation/effect configuration, tearing,
  window/workspace rules, `hyprctl` JSON inspection, plugins, portal, lock,
  idle, wallpaper, picker, launcher, and related tools.

Sources used for the external comparison:

- niri README: https://github.com/niri-wm/niri
- Hyprland wiki: https://wiki.hypr.land/
- Hyprland `hyprctl` reference: https://wiki.hypr.land/Configuring/Using-hyprctl/

## Current Position

JWM is no longer just an X11 WM with a Wayland socket bolted on. The
`wayland-udev` backend already has the shape of a real compositor:

- Smithay udev/libinput/libseat/DRM/GBM/GLES stack.
- XDG shell, layer-shell, xdg-output, decorations, activation, IME, virtual
  keyboard, pointer constraints, relative pointer, pointer gestures, tablet,
  session lock, idle inhibit/notify, presentation timing, fifo, commit timing,
  security context, xdg-foreign, xdg-dialog, system bell, pointer warp, XWayland
  keyboard grab, data-control, KDE decoration, background effect, XWayland.
- Screencopy, ext-image-copy-capture, foreign-toplevel-management,
  ext-workspace, output-management, output-power, gamma-control, tearing-control,
  virtual-pointer, and optional color-management.
- KMS-facing work: dmabuf feedback, direct scanout, VRR/tearing path, HDR
  metadata, scene-linear rendering, KMS gamma/CTM offload hooks, per-monitor
  blur policy, damage tracking, profiling, and metrics.

The gap is therefore not "missing Wayland basics." The gap is how much of this
power is visible, predictable, testable, and delightful in daily use.

## Comparison Matrix

| Area | niri signal | Hyprland signal | JWM now | Evolution target |
| --- | --- | --- | --- | --- |
| Core identity | Scrollable tiling where new windows do not disturb old sizes | Dynamic tiling with several layouts, visible motion, fast customization | Many layouts and tags, plus a Wayland scrolling module | Promote scrolling/overview to a first-class Wayland workflow with per-output state and gesture navigation |
| Multi-monitor | Separate monitor strips, preserved workspace placement, mixed DPI focus | Rich monitor configuration and inspection | KMS outputs, xdg-output, output-management, per-output state | Add monitor persistence, rollback, and diagnostics after modeset/apply |
| Control surface | `niri msg`/config-oriented workflow | `hyprctl` exposes monitors, workspaces, clients, devices, config errors, rolling logs, JSON | `jwm-tool msg`, metrics, daemon tools | Add `wayland-status --json` covering protocols, outputs, KMS caps, latency, scanout, tearing, color, lock |
| Visual system | Overview, blur, animations, custom shader support | Strong animation/effect/rule culture | Blur, transitions, overview, shaders, HDR/color path | Tie effects to performance budgets and expose rejection/degrade reasons |
| Game path | Stable compositor behavior and input latency focus | Tearing, VRR, direct/game-oriented knobs | Direct scanout, VRR, tearing hints, pointer constraints | Report direct-scanout rejection reasons and frame pacing per output |
| Capture/portal | Screen/window capture and sensitive-window masking | Portal ecosystem is expected | jwm-portal plus screencopy/image-copy-capture | Add capture smoke tests and per-client capture policy |
| Config reload | Live reload as normal workflow | Live reload and huge rule surface | Config reload exists; Wayland globals and output/color policy are now visible in status | Keep reducing env-only knobs and expose reload diffs |
| Reliability | Property tests, profiling, input-latency measurement | Large user base catches compatibility issues | Metrics docs and many subsystem tests | Add reproducible Wayland client matrix and regression reports |

## Priority Plan

### P0: Make State Observable

The fastest way to surpass the reference compositors is not another hidden
protocol. It is a better operator surface.

- Add `jwm-tool wayland-status --json`.
- Add the compositor-side `get_wayland_status` IPC query so tooling can fetch
  one coherent status snapshot instead of racing several smaller queries.
- Include active backend, socket, protocol globals enabled/disabled, outputs,
  mode/scale/refresh/HDR/VRR, KMS color caps, dmabuf feedback availability,
  direct-scanout active plus last rejection reason, tearing hint count,
  session-lock state, color-managed surface count, capture queues, and core
  frame/input latency metrics.
- Keep the existing `wayland-audit` command as the human roadmap summary.

### P1: Turn Scrolling Layout Into Identity

JWM should not beat niri by copying every detail; it should make scrolling fit
JWM's tag/layout model.

- Persist per-monitor scroll offset and focused column/window.
- Wire touchpad swipe into column/workspace navigation with client gesture
  suppression only when the WM claims the gesture.
- Show scroll strips in overview so users understand where windows live.
  `get_wayland_status` now exposes an overview-ready scrolling strip per
  monitor, including normalized column positions, focused column, window order,
  and viewport offset. Opening overview in scrolling layout now orders windows
  by left-to-right scrolling columns and keeps the currently focused window as
  the selected overview entry. The Wayland compositor overview now consumes the
  same normalized geometry to draw a bottom scroll strip with column widths and
  per-window ticks, so IPC diagnostics and the visual overlay describe the same
  scrolling identity.
- Add rules for fixed-width columns, centered focused column, and per-app
  default column size.
  Scrolling columns already support per-column width factors and centered
  focused columns; `behavior.scrolling_column_width_rules` now lets users set
  per-app default column width factors with `factor:pattern` rules applied when
  a new window creates a new scrolling column.

### P2: Hyprland-Class Control Plane

Hyprland's daily-use advantage is that users can ask the compositor what is
happening. JWM should expose more raw truth than Hyprland, especially around
KMS and latency.

- Add JSON queries for outputs, clients, devices, protocols, frame stats,
  render decisions, and config parse/reload errors.
  `get_wayland_status` now includes config path/existence/mtime plus reload
  attempt count, last success/error, and last-attempt timestamp; `jwm-tool`
  prints the same summary for quick config-error triage.
- Add a batch command path for config changes and dispatchers.
  `set_config_batch` now applies multiple hot-tunable config overrides
  atomically from one IPC command, with a single config apply and
  `config/changed` broadcast. `command_batch` now runs multiple dispatcher
  commands sequentially from one IPC request, returns per-command results, and
  stops on the first error by default.
- Report why a visual optimization did not activate: direct scanout blocked by
  overlay, blur disabled by budget, HDR offload unsupported, tearing rejected by
  output state, and so on.
  `render_decisions` now aggregates direct-scanout blockers, blur activation,
  HDR output capability, tearing hints, and color-pipeline shader fallback into
  one JSON object, with a concise `jwm-tool wayland-status` summary.
- Move optional Wayland globals and output/color policy into config.
  `behavior.wayland_enable_*` now configures screencopy, tearing-control,
  color-management protocol, output-management, output-power, workspace,
  image-copy-capture, gamma-control, foreign-toplevel-management, and
  virtual-pointer globals from `config_wayland.toml`; legacy `JWM_ENABLE_*`
  variables and `JWM_OPTIONAL_GLOBALS=1` still act as compatibility overrides.
  `wayland-status` reports config/env enablement for optional protocols.

### P3: Wayland Smoke Matrix

Daily-driver trust comes from repeatable client behavior.

- Test native clients: `foot`, GTK, Qt, Electron, SDL/Vulkan game, Waybar,
  wofi/rofi-wayland, grim/slurp, OBS/wf-recorder, wlsunset/gammastep, kanshi.
  `jwm-tool wayland-smoke` now provides a non-invasive preflight matrix for
  these targets: it checks PATH availability, session environment, IPC socket,
  and whether `get_wayland_status` exposes protocols, render decisions,
  capture, output-management, and presentation timing. Each target now declares
  required Wayland protocols and the matrix reports published/missing protocol
  coverage from the live protocol catalog.
- Test XWayland clients: xterm, Steam, legacy tray/window types, fullscreen
  game, clipboard bridge.
  The smoke matrix now includes XWayland readiness from live compositor status:
  XWM ready state, DISPLAY, mapped X11 window count, pending associations, and
  dedicated targets for xterm/xeyes, legacy window types, Steam/fullscreen
  games, tray helpers, and X11/Wayland clipboard bridge tools.
- Store screenshot hashes, protocol availability, frame metrics, and logs.
  `jwm-tool wayland-smoke --save [DIR]` now writes a timestamped JSON report
  containing protocol coverage, target availability, frame/status snapshots,
  render decisions, capture/presentation summaries, recent daemon log tail,
  artifact metadata, a reserved screenshot-hash schema, and manual KMS checklist
  items. Screenshot hashes remain reserved for the future invasive GUI runner,
  but the report schema now defines the hash algorithm and per-capture fields.
- Run under `wayland-winit` for nested CI where KMS is unavailable, then keep a
  manual KMS checklist for DRM-specific behavior.
  Smoke reports now include a `ci_profile` that recommends `wayland-winit` when
  nested Wayland is available and marks KMS as required for full coverage. The
  embedded manual checklist names the output transaction, direct scanout,
  VRR/tearing, HDR/color, capture dmabuf, and XWayland fullscreen evidence to
  collect during real DRM runs.

### P4: Output Configuration With Rollback

The current output-management path can soft-disable and configure outputs. To
feel robust next to niri and Hyprland it needs transaction semantics.

- Snapshot the old layout before applying a wlr-output-management config.
- Apply all requested changes and verify at least one usable output remains.
- Ack only after success, rollback on failure, and expose the failed connector
  and DRM property in diagnostics.
- Persist monitor identity using EDID/vendor/model/serial plus connector name.

### P5: Color/HDR As A Differentiator

JWM has unusually ambitious color plumbing for a small compositor. Make that a
visible advantage.

- Default-safe `wp-color-management-v1` policy with clear enablement.
- Per-output color diagnostic: EDID primaries, HDR metadata, selected transfer
  function, KMS LUT/CTM support, shader fallback.
- Per-surface color debug query and screenshot test scene.
- Explicit policy for SDR-on-HDR and mixed-HDR multi-monitor sessions.
  `wayland-status` now reports a color session policy summarizing HDR vs SDR
  output counts, mixed-HDR detection, SDR-on-HDR handling, mixed-output policy,
  scene-linear enablement, and blockers that explain why the compositor is on a
  safer fallback path.

## CLI Hook

Use the lightweight audit command when deciding what to build next:

```sh
jwm-tool wayland-audit
jwm-tool wayland-audit --markdown
jwm-tool wayland-status
jwm-tool wayland-status --json
```

`wayland-audit` is intentionally static: it is the strategic comparison.
`wayland-status` is the live side. It first asks the compositor for
`get_wayland_status`; if connected to an older compositor it falls back to the
older individual IPC diagnostics. The current snapshot covers outputs,
workspaces, windows, protocol-gate state, metrics, HDR, tearing hints, session
lock, color-managed surfaces, blur status, per-output VRR, and KMS color-pipeline
caps. It also reports direct-scanout state and rejection reasons from both the
compositor scene check and the KMS output check, plus per-output presentation
timing for refresh intervals, pending page flips, watchdog thresholds, and last
vblank age. It also reports wlr-output-management transaction status: pending
acks, soft-disabled outputs, last Apply result, failed outputs, and rollback
attempts. The protocol section now includes a catalog of advertised globals,
whether each is published, whether bind counts are tracked, and runtime bind
counts plus last-bound timestamps for JWM-owned Wayland globals. Output
diagnostics now include a stable monitor identity assembled from connector name
plus EDID vendor/product/serial and descriptor text when the DRM connector
exposes EDID. Output-management transactions also retain before/after output
snapshots so failed applies and rollbacks can be compared without reconstructing
state from logs. Apply/Test validation rejects configurations that would leave
the session with no enabled outputs, and `wayland-status` reports the last
rejected Apply/Test with structured output name, field, DRM property, requested
value, and reason. Failed Apply transactions also expose the first failing
output/property in the human summary. Per-output color diagnostics report the
selected color policy, transfer function, primaries, luminance values,
render-path gate, and whether a shader fallback is required because KMS CTM/LUT
offload is unavailable. Per-surface color diagnostics now keep the raw
wp-color-management parametric values while also reporting readable
transfer-function/primaries names, HDR surface counts, distribution summaries,
max luminance peaks, and sample surfaces in `wayland-status`. Color session
policy now spells out SDR-on-HDR and mixed-HDR behavior plus blockers for
advanced color, render-path, or scene-linear gates. Capture
diagnostics report screencopy and ext-image-copy-capture queue depths, dmabuf
advertising state, cursor-capture support, and the current visible-content
capture policy; runtime counters record queued, fulfilled, dispatch-failed, and
render-failed captures and split modern image-copy requests by output vs
toplevel source, with last queued/fulfilled/failed timestamps and the latest
failure reason for smoke-test triage. The scrolling section now includes
overview strip geometry and overview ordering so tools and the compositor can
show users where each scrolling column lives. Scrolling status also reports
default column-width rule counts so users can confirm per-app width policy is
loaded. Optional Wayland globals are now config-driven through
`behavior.wayland_enable_*`, and protocol diagnostics include config keys plus
environment override state. Smoke reports now carry structured artifact
metadata, a reserved screenshot-hash schema, CI profile guidance, and a manual
KMS checklist so regression evidence can be compared without reverse-engineering
the report format.
