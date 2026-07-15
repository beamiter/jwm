# gpui_component_bar

`gpui_component_bar` is a `gpui-component` rewrite of the original `gpui_bar`.

## Features

- 9 workspace buttons with occupancy, selected, filled, and urgent states
- layout toggle plus 3 layout selection actions
- CPU, memory, and battery usage chips
- brightness and volume controls with left/right click actions
- screenshot launcher via `flameshot gui`
- time display with seconds toggle
- monitor indicator and scale chip
- provider state, WM snapshots, and typed commands through `xbar_core::BarRuntime`
- nonblocking shared transport polling with bounded reconnect after WM restarts

## Build

```bash
cargo check
cargo run -- /path/to/shared-ring-buffer
```
