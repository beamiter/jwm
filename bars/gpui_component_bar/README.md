# gpui_component_bar

`gpui_component_bar` is a `gpui-component` rewrite of the original `gpui_bar`.

## Features

- 9 workspace buttons with occupancy, selected, filled, and urgent states
- layout toggle plus 3 layout selection actions
- CPU, memory, and battery usage chips
- brightness and volume controls with left/right click actions
- screenshot launcher via jwm's built-in region capture (IPC `take_screenshot`)
- time display with seconds toggle
- monitor indicator and scale chip
- provider state, WM snapshots, and typed commands through `xbar_core::BarRuntime`
- nonblocking shared transport polling with bounded reconnect after WM restarts

## Build

```bash
cargo check
cargo run -- /path/to/shared-ring-buffer
```

## Framework compatibility

The GPUI component toolkit and the native platform must use the same GPUI
snapshot: this bar pairs `gpui-component 0.6.6` with `gpui-pre 0.3.6` and
`gpui-pre-platform 0.3.6`. Update these together after checking all targets.
The older `gpui 0.2.2` is a different package with incompatible component types;
using the aligned packages also removes its obsolete xattr dependency.
