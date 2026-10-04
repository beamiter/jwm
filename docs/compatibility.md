# Compatibility and support

JWM is under active development and has no stable published release. This is the
current development/testing contract, not a production-support promise.

## Platform matrix

| Surface | Current status | Important gaps |
| --- | --- | --- |
| Wayland DRM/KMS | **Primary production backend** | Needs DRM/GBM/EGL, input, seat permissions, and real-hardware validation ([hardware-validation](hardware-validation.md)) |
| Nested Wayland | CI/development smoke backends | Not a production DRM/KMS substitute; capture is absent where unsupported; no shell panels or lock screen; wlr-output-management (kanshi/wlr-randr) is advertised only on DRM/KMS |
| X11RB | First-class compatibility; integrated compositor | Inherits X11's session-wide trust model; server/driver extensions vary |
| XCB | Differential policy coverage with X11RB | Parity tests cannot cover every server, extension, or GPU |
| XWayland | Available in Wayland sessions | Inherits X11 isolation limits and application/driver quirks |
| Portal/bars | Optional, separate components | Portal needs PipeWire 1.2 metadata; some toolkit bars have narrower gates |

The shell panels — the control center and its pickers, the window switcher,
the tags overview, the lock screen — draw through JWM's own compositor, and
the nested Wayland backends cannot start one, so on those they refuse to open
rather than half-appearing. Everything that does not need a panel (layouts,
tags, keybindings, IPC) works there, which is what makes them useful for
development.

Native xdg fullscreen and minimize requests use the shared window policy, as do
XWayland fullscreen, minimize and activation requests. Fullscreen uses the
window's current monitor; the optional xdg output hint is not applied. Activation
can reveal another tag or restore a minimized window, and xdg activation keeps
the existing ten-second token freshness check.

Maximize requests from native X11, xdg-shell, XWayland and wlr-foreign-toplevel
use the same shared policy. A maximized window fills its monitor's work area —
the monitor minus the status bar, docks and the tab bar — with its border inside
that area and no gaps. What each protocol can express differs:

| Protocol | Maximize support |
| --- | --- |
| Native X11 | Per axis: `_NET_WM_STATE_MAXIMIZED_VERT` and `_HORZ` are independent, and one message naming both is a single request. EWMH source indication (`data[3]`) is honored: pager (2) may promote a tiled window; application/unspecified (0/1) may not |
| xdg-shell | Both axes only; `Maximized` is reported only while both are set, and every set/unset request gets exactly one configure, a refused one included |
| XWayland | Only the paired request; `Maximized` is published only for both axes, and a maximized window cannot move or resize itself |
| wlr-foreign-toplevel | Both axes; taskbar set/unset requests go through the same policy and are treated as pager/user origin (may promote a tiled window), and managed XWayland windows are listed and accept them too |

A maximize that an application (or xdg/XWayland) requests, or that a window
already carries when JWM starts managing it, is refused for a window the
tiling layout manages, as in sway: the current state is republished, so the
client does not believe a maximize that did not happen, and pre-set maximized
atoms are cleared. An EWMH pager with source indication 2, and a
wlr-foreign-toplevel taskbar `set_maximized`, may promote a tiled window like
`togglemaximize`. The `togglemaximize` command takes a tiled window out of the
layout while it is maximized, and toggling again puts it back.
Floating windows and every window under the float layout accept client
requests; fixed-size windows and docks are never maximized. Native X11,
XWayland and xdg-shell interactive move/resize requests feed the shared drag
pipeline and unmaximize in place. See
[window placement](window-placement.md#maximize).

Managed-client Above/Below is handled consistently for XWayland and native X11
policy: conflicting flags resolve to Above, property writes are echoed back,
and the managed stack uses `Below < Normal < Above < focused fullscreen < PiP`.
`get_windows` / `get_clients` / `window/state` expose `is_above` and `is_below`, plus
`is_swallowed`, `is_on_view`, `is_scratchpad`, `border_w`, optional
`scratchpad` / `layout`, chrome / size-hint flags (`is_fixed`,
`is_dock`, `is_desktop`, `is_drag_floating`, `never_focus`,
`skip_taskbar`, `skip_pager`, `no_decorations`, `demands_attention`,
`has_strut`, `client_fact`), resting `float_rect`, and `old_geometry`.
`get_monitors` / `get_tree` report the work area as `wx` / `wy` / `ww` /
`wh`, plus `scale`, `refresh_mhz`, `hdr_capable`, optional `hdr_metadata`,
tiling `gap` / `m_fact` / `n_master`, `transform`, physical size
(`physical_width_mm` / `physical_height_mm`), and preferred mode
(`preferred_width` / `preferred_height` / `preferred_refresh_mhz`).
`get_config` accepts optional `keys` to return a field subset, including
`hdr_enabled`, `idle_dim_secs` / `idle_dim_level`, `night_light*` schedule
fields, `remember_closed_placement`, WaterLily env mirrors
(`waterlily_enabled` / `waterlily_opacity`), expose/peek/tags/magnifier/
tabs/VRR/swallow/idle/wallpaper/opacity/blur/recording keys, and polish
keys (`clipboard_history`, `border_glow_*`, `resource_rows`,
`new_client_position`, `compositor_api`, …) plus further polish sets
(border colors / gradient, edge glow, tilt, ripple, wallpaper crossfade,
wobbly details, wayland_enable_*, power commands, color grading,
particles, …). `get_tree` nodes also carry `selected_id`, `window_count`,
urgency / floating / minimized / sticky / fullscreen / pip / maximized /
above / below / scratchpad / tabbed / fixed / dock / desktop /
never_focus / demands_attention / skip_taskbar / skip_pager /
no_decorations / drag_float / swallowed / on_view / maximize_promoted /
strut / status_bar counts. `get_workspaces`
includes per-tag `gap` plus matching count fields (including scratchpad /
tabbed / dock / desktop / skip_* / drag_float / swallowed / on_view /
maximize_promoted / strut / status_bar); `get_monitors` report the
same count family plus `lt_symbol` / optional `output_id`. `get_layout` /
`get_gaps` / `get_nmaster` return the focused monitor's live layout
parameters; `setgaps` / `setnmaster` / `set_nmaster` / `set_layout` are
bindable over IPC. Window rows also report border-inclusive `total_w` /
`total_h` and optional `stack_index`. `get_status` nests compact tearing /
xwayland / scrolling / color_management / audio / wallpaper / bluetooth /
system_ui / layout / tabs / struts / scratchpads / gaps / mfact / nmaster /
show_bar / metrics / version_info / monitors / workspaces / windows / tree /
focused / cfact / prev_layout / effects / mic / capabilities / selected /
bench / floating / minimized / sticky / urgent / fullscreen / pip
summaries alongside earlier nests. `get_effect_status` /
`get_effects` reports shell picker flags, magnifier radius, and
`compositor_active`; short query aliases include `get_notif`, `get_ui`,
`get_lock`, `get_tabs`, `get_clip`, `get_network`, `get_tearing`,
`get_scrolling`, `get_scroll`, `get_xwayland`, `get_do_not_disturb`, `get_caps`,
`get_pads`, `get_mag`, `get_perf`, `get_res`, `get_wins`, `get_devices`,
`get_cfg`, `get_ver`, `get_bt`, `get_wl`, `get_nl`, `get_cm`, `get_sess`,
`get_strut`, `get_scratch`, `get_mons`, `get_ws`, `get_gap`, `get_nm`,
`get_mf`, `get_tab`, `get_bench`, `get_gest`, `get_wall`, `get_lt`,
`get_cf`, `get_sel`, `get_fw`, `get_pl`, `get_bar`, `get_tr`, `get_win`,
`get_conn`, `get_st`, `get_fx`, `get_mute`, `get_cli`, `get_wc`, `get_th`,
`get_pair`, `get_pk`, `get_bm`, `get_conf`, `get_rec`, `get_arec`,
`get_cap`, `get_xw`, `get_wly`, `get_idl`, `get_cp`. Command aliases
include `launcher`, `notif_center`, `screenshot`, `lock`, `layouts`,
`load_session`, `toggle_do_not_disturb`, `hub`, `switcher`, `tags`,
`overview`, `peek`, `mag`, `annotate`, `lily`, `night`, `caffeine`, `wifi`,
`bt`, `wall`, `session`, `floating`, `sticky`, `pip`, `maximize`, `cal`,
`clip`, `monlayout`, `aout`, `ain`, `unlock`, `snap`, `record`, `arecord`,
`bar`, `comp`, `play`, `next`, `prev`, `stop`, `unfocus`, `damage`,
`cycle`, `kill`, `last`, `loop`, `save`, `restore`, `pad`, `ftab`, `fwin`,
`case`, `palette`, `region`, `attach`, `scol`, `smov`, `swin`, `scons`,
`sexp`, `twifi`, `tbt`, `clayout`. Session snapshots are at v17
(`hidden_x`). Saved parking coordinates are diagnostic: restore recalculates
them for the current output topology. Minimized and fullscreen return rectangles
are clamped to the current monitor work area. New snapshots omit unset geometry;
historical old-geometry slots with both dimensions zero remain readable as unset,
while negative or partially zero dimensions are rejected. `get_magnifier` /
`get_mag` reports `radius`; `get_peek` reports `compositor_active`;
`get_tab_bar` / `get_tabs` reports `selected_id` when a tab group is focused.
Layer-shell background/top/overlay surfaces remain compositor-owned layers.

The binary-bundle design currently targets **x86_64 Linux built on Ubuntu
22.04**. The host must provide compatible graphics, input, seat, audio, D-Bus,
and font libraries. Other distributions/architectures should build the tagged
source with Rust 1.89 or newer. No binary promise exists for ARM, musl, BSD,
macOS, or Windows.

Hosted CI cannot certify a kernel/GPU/driver combination. Run `jwm --backend
wayland-udev --doctor` and validate modeset, hotplug, suspend/resume, VT switch,
multi-monitor, capture, and rendering on real hardware. HDR remains conditional
where output coherence is not guaranteed; VRR, direct scanout, color management,
and EGL/GBM behavior remain driver-sensitive. Live output layouts that would
grow or shrink the global framebuffer envelope are refused until a full KMS
reinit — see [output layout](output-layout.md).

## Variable refresh rate (VRR)

`[behavior]` carries `vrr_enabled` (default true), `vrr_min_fps` (30),
`vrr_max_fps` (240), and `game_classes` (window classes treated as games). What
they do depends on the backend:

- **Wayland DRM/KMS:** a per-output presentation policy runs once per frame.
  `VRR_ENABLED` is asserted while a mapped fullscreen window *covers that
  output* and cleared otherwise; VRR on a static desktop makes some panels
  flicker, so it follows the content. It follows only the content: a cursor
  moving over the game, an overlay, or a colour-delivery retry do not change
  it, because each change costs a mode-size test buffer and a test commit and
  is a visible refresh renegotiation on many panels.

  It is programmed through Smithay's own `use_vrr`, only on connectors that
  report VRR can change without a modeset, and only when the value differs
  from the last one attempted — a driver that refuses a value is not asked
  again until the wanted value changes. A toggle Smithay could satisfy only
  through its modeset fallback is undone again and VRR control is withdrawn
  from that output for the rest of the session: the probe said no modeset was
  needed, the commit said otherwise, and a full modesetting commit per
  fullscreen window is worse than no VRR at all. `get_wayland_status` reports
  each output's capability under `outputs[].vrr`: `supported` comes from the
  same probe that decides whether VRR is programmed at all, so it never
  claims support the policy would then decline to use, and `current_enabled`
  is the CRTC's `VRR_ENABLED` value. What `render_decisions.vrr` reports as
  VRR-active is what the last `use_vrr` actually took rather than what it was
  asked for.

  VRR is not switched by hand over IPC: no command toggles or forces it, and
  the only controls are `vrr_enabled` and the fullscreen content rule above.
  The backend does keep a per-output override that the policy reads, so a
  future control can latch a request instead of programming the hardware
  directly (the next rendered frame would otherwise recompute VRR from content
  and undo it), but nothing exposes it yet.

  (Before this, VRR was written straight onto the CRTC property at output
  init and by the backend's enable path, and never survived: Smithay
  re-asserts its own cached VRR value in every atomic request it builds, so
  the enable was undone by the very next page flip while the call reported
  success.)

  `wp_tearing_control_manager_v1` is published when
  `wayland_enable_tearing_control` is on (default true). Hints are
  double-buffered and latched at `wl_surface.commit`, and the same policy
  decides per output whether the frame would be flipped asynchronously —
  but it never is, and the reason is reported rather than implied: JWM's
  frame submission goes through Smithay's `DrmCompositor::queue_frame`, whose
  submission step hardcodes its atomic commit flags and offers no way to
  request `PAGE_FLIP_ASYNC`. `jwm-tool msg get_tearing_hints` and
  `render_decisions.tearing` name that blocker
  (`submission_cannot_request_async_flip`) per output, alongside the ones
  that would apply anyway: a driver without
  `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`, a frame that needs composition, a
  colour-delivery retry, or pending surface state that forces a modesetting
  commit.
- **X11:** the X server owns DRM master, so no per-output VRR toggle is
  reachable through RandR or any X extension; the X11 backends therefore
  refuse a VRR toggle with an explicit "unsupported" error instead of
  pretending, and JWM never switches VRR itself. While `vrr_enabled` is on,
  `get_wayland_status` reports `outputs[].vrr.supported` from the output's
  RandR `vrr_capable` property, with `current_enabled` always false. The flags
  only drive the HUD/metrics "VRR active" indicator. Real VRR for games comes
  from `fullscreen_unredirect` (default true) letting the game present
  directly, plus driver-side configuration such as amdgpu's `VariableRefresh`
  xorg.conf option.

## Independent component SemVer

The root `jwm`, bridge, portal, shared protocol, bar core/providers, and every
bar own their SemVer number. `jwm-v0.2.0` names a root JWM release and exact
source bundle; it does not imply every component is version 0.2.0. Before a
component reaches 1.0, minor versions may break compatibility. After 1.0,
breaking public changes require a major version.

## Schema and deprecation policy

- **Configuration:** additive keys with defaults are compatible. Renames,
  removals, type/meaning changes, or stricter validation require a warning and
  continued loading of the old form for at least one JWM minor-release cycle.
- **IPC:** additive commands/topics/fields are compatible and clients must
  ignore unknown object fields. Removing or changing commands, required
  arguments, response meaning, or events requires a warning/capability signal
  for at least one minor cycle. Query `jwm-tool capabilities --json`.
- **Versioned JSON:** incompatible envelope/field changes increment
  `schema_version`; readers must reject unsupported future versions.
- **Sessions:** JWM reads the current and at least previous format through a
  tested in-memory migration. It does not rewrite the old file until a later
  atomic save. A format is removed only after at least two minor cycles and the
  changelog identifies the last reader.

Cycle counts begin with the first published release introducing a deprecation.
Urgent security removals may shorten a window, but require an advisory,
changelog entry, and migration or disablement path.

Report failures with exact component versions, backend, distribution, kernel,
GPU, driver, and renderer. Review support bundles before sharing them; use the
private process in [SECURITY.md](../SECURITY.md) for sensitive failures.
evolve9h wave 18: Health compact `version_info` is diagnosable through `jwm-tool health`.
evolve9h wave 19: Health compact `metrics` is diagnosable through `jwm-tool health`.
evolve9h wave 20: Health compact `ipc_caps` is diagnosable through `jwm-tool health`.
evolve9h wave 21: Health compact `config` is diagnosable through `jwm-tool health`.
evolve9h wave 22: Health compact `status` is diagnosable through `jwm-tool health`.
evolve9h wave 23: Health compact `tree` is diagnosable through `jwm-tool health`.
evolve9h wave 24: Health compact `window` is diagnosable through `jwm-tool health`.
evolve9h wave 25: Health compact `session_lock` is diagnosable through `jwm-tool health`.
evolve9h wave 26: Health compact `tearing` is diagnosable through `jwm-tool health`.
evolve9h wave 27: Health compact `xwayland` is diagnosable through `jwm-tool health`.
evolve9h wave 28: Health compact `scrolling` is diagnosable through `jwm-tool health`.
evolve9h wave 29: Health compact `night_light` is diagnosable through `jwm-tool health`.
evolve9h wave 30: Health compact `magnifier` is diagnosable through `jwm-tool health`.
evolve9h wave 31: Health compact `peek` is diagnosable through `jwm-tool health`.
evolve9h wave 32: Health compact `gesture` is diagnosable through `jwm-tool health`.
evolve9h wave 33: Health compact `wayland` is diagnosable through `jwm-tool health`.
evolve9h wave 34: Health compact `recording` is diagnosable through `jwm-tool health`.
evolve9h wave 35: Health compact `capture` is diagnosable through `jwm-tool health`.
evolve9h wave 36: Health compact `waterlily` is diagnosable through `jwm-tool health`.
evolve9h wave 37: Health compact `audio` is diagnosable through `jwm-tool health`.
evolve9h wave 38: Health compact `wallpaper` is diagnosable through `jwm-tool health`.
evolve9h wave 39: Health compact `bluetooth` is diagnosable through `jwm-tool health`.
evolve9h wave 40: Health compact `resources` is diagnosable through `jwm-tool health`.
evolve9h wave 41: Health compact `connectivity` is diagnosable through `jwm-tool health`.
evolve9h wave 42: Health compact `power` is diagnosable through `jwm-tool health`.
evolve9h wave 43: Health compact `media` is diagnosable through `jwm-tool health`.
evolve9h wave 44: Health compact `clipboard` is diagnosable through `jwm-tool health`.
evolve9h wave 45: Health compact `idle` is diagnosable through `jwm-tool health`.
evolve9h wave 46: Health compact `notifications` is diagnosable through `jwm-tool health`.
evolve9h wave 47: Health compact `dnd` is diagnosable through `jwm-tool health`.
evolve9h wave 48: Health compact `system_ui` is diagnosable through `jwm-tool health`.
evolve9h wave 49: `jwm-tool capabilities` remains the catalog for `version_info` query aliases.
evolve9h wave 50: `jwm-tool capabilities` remains the catalog for `metrics` query aliases.
evolve9h wave 51: `jwm-tool capabilities` remains the catalog for `ipc_caps` query aliases.
evolve9h wave 52: `jwm-tool capabilities` remains the catalog for `config` query aliases.
evolve9h wave 53: `jwm-tool capabilities` remains the catalog for `status` query aliases.
evolve9h wave 54: `jwm-tool capabilities` remains the catalog for `tree` query aliases.
evolve9h wave 55: `jwm-tool capabilities` remains the catalog for `window` query aliases.
evolve9h wave 56: `jwm-tool capabilities` remains the catalog for `session_lock` query aliases.
evolve9h wave 57: `jwm-tool capabilities` remains the catalog for `tearing` query aliases.
evolve9h wave 58: `jwm-tool capabilities` remains the catalog for `xwayland` query aliases.
evolve9h wave 59: `jwm-tool capabilities` remains the catalog for `scrolling` query aliases.
evolve9h wave 60: `jwm-tool capabilities` remains the catalog for `night_light` query aliases.
evolve9h wave 61: `jwm-tool capabilities` remains the catalog for `magnifier` query aliases.
evolve9h wave 62: `jwm-tool capabilities` remains the catalog for `peek` query aliases.
evolve9h wave 63: `jwm-tool capabilities` remains the catalog for `gesture` query aliases.
evolve9h wave 64: `jwm-tool capabilities` remains the catalog for `wayland` query aliases.
evolve9h wave 65: `jwm-tool capabilities` remains the catalog for `recording` query aliases.
evolve9h wave 66: `jwm-tool capabilities` remains the catalog for `capture` query aliases.
evolve9h wave 67: `jwm-tool capabilities` remains the catalog for `waterlily` query aliases.
evolve9h wave 68: `jwm-tool capabilities` remains the catalog for `audio` query aliases.
evolve9h wave 69: `jwm-tool capabilities` remains the catalog for `wallpaper` query aliases.
evolve9h wave 70: `jwm-tool capabilities` remains the catalog for `bluetooth` query aliases.
evolve9h wave 71: `jwm-tool capabilities` remains the catalog for `resources` query aliases.
evolve9h wave 72: `jwm-tool capabilities` remains the catalog for `connectivity` query aliases.
evolve9h wave 73: `jwm-tool capabilities` remains the catalog for `power` query aliases.
evolve9h wave 74: `jwm-tool capabilities` remains the catalog for `media` query aliases.
evolve9h wave 75: `jwm-tool capabilities` remains the catalog for `clipboard` query aliases.
evolve9h wave 76: `jwm-tool capabilities` remains the catalog for `idle` query aliases.
evolve9h wave 77: `jwm-tool capabilities` remains the catalog for `notifications` query aliases.
evolve9h wave 78: `jwm-tool capabilities` remains the catalog for `dnd` query aliases.
evolve9h wave 79: `jwm-tool capabilities` remains the catalog for `system_ui` query aliases.
evolve9h wave 80: Support bundles should include `version_info` when health is degraded.
evolve9h wave 81: Support bundles should include `metrics` when health is degraded.
evolve9h wave 82: Support bundles should include `ipc_caps` when health is degraded.
evolve9h wave 83: Support bundles should include `config` when health is degraded.
evolve9h wave 84: Support bundles should include `status` when health is degraded.
evolve9h wave 85: Support bundles should include `tree` when health is degraded.
evolve9h wave 86: Support bundles should include `window` when health is degraded.
evolve9h wave 87: Support bundles should include `session_lock` when health is degraded.
evolve9h wave 88: Support bundles should include `tearing` when health is degraded.
evolve9h wave 89: Support bundles should include `xwayland` when health is degraded.
evolve9h wave 90: Support bundles should include `scrolling` when health is degraded.
evolve9h wave 91: Support bundles should include `night_light` when health is degraded.
evolve9h wave 92: Support bundles should include `magnifier` when health is degraded.
evolve9h wave 93: Support bundles should include `peek` when health is degraded.
evolve9h wave 94: Support bundles should include `gesture` when health is degraded.
evolve9h wave 95: Support bundles should include `wayland` when health is degraded.
evolve9h wave 96: Support bundles should include `recording` when health is degraded.
evolve9h wave 97: Support bundles should include `capture` when health is degraded.
evolve9h wave 98: Support bundles should include `waterlily` when health is degraded.
evolve9h wave 99: Support bundles should include `audio` when health is degraded.
evolve9h wave 100: Support bundles should include `wallpaper` when health is degraded.
evolve9h wave 101: Support bundles should include `bluetooth` when health is degraded.
evolve9h wave 102: Support bundles should include `resources` when health is degraded.
evolve9h wave 103: Support bundles should include `connectivity` when health is degraded.
evolve9h wave 104: Support bundles should include `power` when health is degraded.
evolve9h wave 105: Support bundles should include `media` when health is degraded.
evolve9h wave 106: Support bundles should include `clipboard` when health is degraded.
evolve9h wave 107: Support bundles should include `idle` when health is degraded.
evolve9h wave 108: Support bundles should include `notifications` when health is degraded.
evolve9h wave 109: Support bundles should include `dnd` when health is degraded.
evolve9h wave 110: Support bundles should include `system_ui` when health is degraded.
evolve9h wave 111: Nested smoke treats `version_info` as a read-only IPC probe.
evolve9h wave 112: Nested smoke treats `metrics` as a read-only IPC probe.
evolve9h wave 113: Nested smoke treats `ipc_caps` as a read-only IPC probe.
evolve9h wave 114: Nested smoke treats `config` as a read-only IPC probe.
evolve9h wave 115: Nested smoke treats `status` as a read-only IPC probe.
evolve9h wave 116: Nested smoke treats `tree` as a read-only IPC probe.
evolve9h wave 117: Nested smoke treats `window` as a read-only IPC probe.
evolve9h wave 118: Nested smoke treats `session_lock` as a read-only IPC probe.
evolve9h wave 119: Nested smoke treats `tearing` as a read-only IPC probe.
evolve9h wave 120: Nested smoke treats `xwayland` as a read-only IPC probe.
evolve9h wave 121: Nested smoke treats `scrolling` as a read-only IPC probe.
evolve9h wave 122: Nested smoke treats `night_light` as a read-only IPC probe.
evolve9h wave 123: Nested smoke treats `magnifier` as a read-only IPC probe.
evolve9h wave 124: Nested smoke treats `peek` as a read-only IPC probe.
evolve9h wave 125: Nested smoke treats `gesture` as a read-only IPC probe.
evolve9h wave 126: Nested smoke treats `wayland` as a read-only IPC probe.
