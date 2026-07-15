# JWM bar consumer matrix

The core boundary and 0.3 compatibility were checked against every bar
repository under `jwm/submodules`. The profiles below describe the 0.3
adoption target: window creation, scale conversion, dock/strut properties, and
concrete drawing stay in each frontend. Semantic state, providers, WM protocol
conversion, reconnect lifecycle, cadence, and typed actions belong to
`xbar_core`.

## Native window and presentation backends

| Consumers | Frontend-owned boundary | 0.3 core profile |
|---|---|---|
| `xcb_bar`, `x11rb_bar` | X connection/window, Cairo surface, epoll | Cairo facade, Linux timer/notifier, all providers, managed transport |
| `xcb_wgpu_bar`, `x11rb_wgpu_bar` | X window plus wgpu surface/upload | Cairo scene/render source, Linux timer/notifier, all providers, managed transport |
| `winit_pixels_bar`, `winit_softbuffer_bar`, `winit_wgpu_bar` | winit lifecycle and pixels/softbuffer/wgpu presentation | Cairo facade, all providers, managed transport |
| `tao_pixels_bar`, `tao_softbuffer_bar`, `tao_wgpu_bar` | tao lifecycle and pixels/softbuffer/wgpu presentation | Cairo facade, all providers, managed transport |

These consumers translate native pointer events into `PointerAction`, apply
geometry effects to their own windows, and optionally rebuild a notifier when
the core transport generation changes.

## Rust toolkit backends

| Consumers | Frontend-owned boundary | 0.3 core profile |
|---|---|---|
| `dioxus_bar`, `egui_bar`, `gpui_bar`, `gpui_component_bar`, `iced_bar`, `xilem_bar` | Widget tree, theme, toolkit window/async tasks | `BarRuntime`, owned `BarSnapshot`, clock and all providers, managed transport, `RuntimeSchedule` |
| `gtk_bar`, `relm_bar` | GTK/Relm widgets and GLib tasks | `BarRuntime`, owned `BarSnapshot`, clock/audio/system, managed transport, `RuntimeSchedule` |

After migration, toolkit-specific popups and styling remain local. Provider
managers, retry deadlines, protocol types, and a second WM availability flag
do not.

## Tauri/web backends

| Consumers | Frontend-owned boundary | 0.3 core profile |
|---|---|---|
| `tauri_leptos_bar`, `tauri_react_bar`, `tauri_solid_bar`, `tauri_svelte_bar`, `tauri_vue_bar`, `tauri_yew_bar` | Tauri commands/events, webview window placement, framework UI | `BarRuntime`, serializable `BarSnapshot`, all providers, managed transport, `RuntimeSchedule` |

The six backends can share the same lifecycle pattern regardless of the web UI
framework. Tauri remains responsible for event naming and window APIs; core
owns authoritative snapshots and typed command validation.

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
