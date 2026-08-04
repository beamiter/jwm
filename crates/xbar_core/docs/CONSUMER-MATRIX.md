# JWM bar consumer matrix

Version 0.6 adds `xbar_present_wgpu` for the four wgpu bars, damage metadata
consumed by softbuffer/wgpu presenters, and the shared `config-toml` appearance
file adopted by all ten native bars; see `MIGRATION-0.6.md`.

Version 0.5 keeps every profile below and additionally hands the native bars
`linux::Epoll`, the `DockWindowSpec` property protocol, CPU-frame rendering
(`render_into_bgra`/`CpuCanvas`), and the `xbar_linux_actions::EffectRouter`
host route; see `MIGRATION-0.5.md` for the per-family adoption map.

The core boundary and 0.4 compatibility were checked against every one of the
24 bar repositories under `jwm/submodules`. The profiles below describe the 0.4
adoption target: window creation, native window/property calls, and concrete
drawing stay in each frontend. Semantic state, providers, WM protocol
conversion, reconnect lifecycle, cadence, display policy, frontend wire state,
and typed actions belong to `xbar_core`.

## Native window and presentation backends

| Consumers | Frontend-owned boundary | 0.4 core profile |
|---|---|---|
| `xcb_bar`, `x11rb_bar` | X connection/window, Cairo surface, epoll registration | Cairo facade, `BarPlacement`/`EwmhStrut`, pointer facade, notifier slot, all providers, managed transport |
| `xcb_wgpu_bar`, `x11rb_wgpu_bar` | X window plus wgpu surface/upload | Cairo scene/render source, placement/strut, pointer facade, notifier slot, all providers, managed transport |
| `winit_pixels_bar`, `winit_softbuffer_bar`, `winit_wgpu_bar` | winit lifecycle and pixels/softbuffer/wgpu presentation | Cairo/pointer facade, placement, owned wake forwarding, all providers, managed transport |
| `tao_pixels_bar`, `tao_softbuffer_bar`, `tao_wgpu_bar` | tao lifecycle and pixels/softbuffer/wgpu presentation | Cairo/pointer facade, placement, owned wake forwarding, all providers, managed transport |

These consumers translate only their native event/window types. Core owns
press/release matching, wheel direction, physical placement/strut values, and
notifier replacement when the transport generation changes.

## Rust toolkit backends

| Consumers | Frontend-owned boundary | 0.4 core profile |
|---|---|---|
| `dioxus_bar`, `egui_bar`, `gpui_bar`, `gpui_component_bar`, `iced_bar`, `xilem_bar` | Widget construction, colors, toolkit window/async tasks | `RuntimeFrame`, geometry-free control projection, display/icon policy, all providers, managed transport, `RuntimeSchedule` |
| `gtk_bar`, `relm_bar` | GTK/Relm widget construction and GLib tasks | `RuntimeFrame`, control projection with custom thresholds/icons, clock/audio/system, managed transport, `RuntimeSchedule` |

After migration, toolkit-specific popups and styling remain local. Tag-state
precedence, canonical layout IDs, unavailable handling, input bindings,
provider managers, retry deadlines, protocol types, and a second WM
availability flag do not.

## Tauri/web backends

| Consumers | Frontend-owned boundary | 0.4 core profile |
|---|---|---|
| `tauri_leptos_bar`, `tauri_react_bar`, `tauri_solid_bar`, `tauri_svelte_bar`, `tauri_vue_bar`, `tauri_yew_bar` | Tauri emit/listen, webview window API, framework UI | `RuntimeFrame`, `FrontendEnvelope`, `SnapshotCursor`, one `ActionRequest`, all providers, managed transport, `RuntimeSchedule` |

The six Rust backends were identical except for the logger name (635 lines
each, 3,810 copied lines total). Tauri remains responsible for event naming and
window APIs; core owns the complete non-lossy snapshot, revisions/diff, replay,
and typed command validation.

## Audit-driven migration debt

- All 10 native bars still carried manual transport open/retry logic and built
  a notifier only at startup; the 0.4 notifier slot closes the reconnect
  low-latency gap while 0.3 managed recovery removes the retry copies.
- All eight toolkit bars still carried local tag/layout/status projections.
  The 0.4 display and control projection APIs make those copies removable and
  fix the previous default layout ID 1/2 reversal.
- All six Tauri backends still carried four DTO mappers and an emitted-state
  cache. The complete envelope preserves `geometry: None`, absent battery, and
  the unmodified layout symbol instead of manufacturing zero geometry, 100%
  battery, or a combined debug string.

## Capability rules

- Every manifest keeps `default-features = false` and selects only the profile
  it consumes.
- `render-cairo` is limited to native scene consumers; toolkit and Tauri bars
  consume model snapshots directly.
- `runtime-linux` is limited to consumers registering timerfd/eventfd handles.
- `transport-shared` is the only dependency path to `shared_structures`.
- Battery and brightness remain independent provider features; GTK/Relm do
  not inherit them accidentally.
- Managed recovery never creates or destroys the WM-owned ring.
