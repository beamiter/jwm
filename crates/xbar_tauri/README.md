# xbar_tauri

Shared Tauri 2 host bridge for JWM bars. It installs one complete state event,
one checked action command, frontend replay, managed transport/provider service,
window placement effects, process actions, and bounded worker shutdown.

```toml
xbar_tauri = { git = "https://github.com/beamiter/xbar_core.git", features = [
  "clock-chrono", "provider-alsa", "provider-battery-sysfs",
  "provider-brightnessctl", "provider-system",
] }
```

```rust,ignore
let shared_path = std::env::args().nth(1).unwrap_or_default();
let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
let builder = xbar_tauri::configure(builder, xbar_tauri::BridgeConfig::new(shared_path))?;
builder.run(tauri::generate_context!())?;
```

The frontend listens to `xbar-state`, invokes `dispatch_action` with a
`request` argument containing one internally tagged `ActionRequest`, and
invokes `frontend_ready` after installing its listener:

```ts
await invoke("dispatch_action", {
  request: { action: "view_tag_on", tag_index: 2, monitor_id: 0 },
});
await invoke("frontend_ready");
```
