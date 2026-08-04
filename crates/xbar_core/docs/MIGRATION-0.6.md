# Adopting 0.6 presentation, damage, and desktop-service layers

Version 0.6 keeps every 0.5 surface. It adds the GPU presenter companion, damage
metadata on CPU frames, a shared TOML configuration, Wayland layer-shell
placement data, a D-Bus provider companion, and wire-protocol property tests.
Releases are now tagged: pin `tag = "v0.6.0"` instead of a commit hash.

## `xbar_present_wgpu` companion

The four wgpu bars carried identical ~300-line surface/upload/blit copies.
`WgpuPresenter` owns surface configuration, sRGB format choice, BGRA/RGBA
upload conversion, 256-byte row alignment, outdated/lost surface recovery, and
the fullscreen blit:

```rust,ignore
use xbar_present_wgpu::{PresentRect, WgpuPresenter};

let mut presenter = WgpuPresenter::new_blocking(window.clone(), width, height)?;
presenter.resize(new_width, new_height);

let frame = canvas.render(&mut bar, width, height, scale)?;
presenter.present_bgra(frame.data, frame.stride, damage_for(&frame))?;
```

`new` accepts any `Into<wgpu::SurfaceTarget<'static>>`: an `Arc<Window>` for
winit/tao or a raw-window-handle wrapper for XCB/x11rb. The presenter depends
on wgpu 30 and nothing from a UI framework.

## Damage-aware CPU frames

`CairoBar` now records `last_damage()` — the logical-coordinate union of what
changed between the two most recent scenes — and `CpuFrame` carries it as a
device-pixel rect:

- `frame.damage == None`: first frame or resize; present everything.
- `Some(rect)` non-empty: only `rect` changed.
- `Some(rect)` empty: the scene was visually identical; presenters can skip
  the upload.

`WgpuPresenter::present_bgra` takes the damage directly. Softbuffer bars keep
their full-frame copy but call `present_with_damage` so the server blits only
the changed region. Pixels bars are unchanged: that presenter always uploads
the full buffer.

## Shared TOML configuration (`config-toml`)

`config::BarConfig` replaces the per-bar `XBAR_FONT` handling and hard-coded
`PresentationConfig`:

```rust,ignore
let config = xbar_core::config::BarConfig::load_default()?;
let runtime = BarRuntime::with_managed_transport(config.model_config(), recovery)?;
let mut bar = CairoBar::new(runtime, config.presentation, FontDescription::from_string(&config.font));
if let Some(opacity) = config.background_opacity {
    bar.renderer_mut().set_background_opacity(Some(opacity));
}
```

Resolution order: `$XBAR_CONFIG` (must exist when set), else
`$XDG_CONFIG_HOME/xbar/config.toml`, else `~/.config/xbar/config.toml`; a
missing file is exactly the defaults, and `XBAR_FONT` still overrides the font
so existing setups keep working. Every field is optional; invalid values and
unknown keys are startup errors, never silent fallbacks:

```toml
font = "JetBrainsMono Nerd Font 12"
theme = "dark"
background_opacity = 0.9

[presentation]
bar_height = 40.0
font_size = 13.0
tag_labels = ["一", "二", "三"]
```

## Wayland layer-shell placement data

`placement::LayerShellPlacement::top(namespace, logical_height)` mirrors
`DockWindowSpec` for wlr-layer-shell: layer, anchors, exclusive zone, margins,
and logical height as pure data. A compositor-side companion crate is
deliberately deferred until a first Wayland consumer exists (the same
extraction threshold every companion followed); the protocol values a frontend
must pass are already centralized here.

## `xbar_dbus_providers` companion

`UPowerBatteryProvider` polls UPower's display device over the system bus and
reduces it to the existing `BatteryState` — an alternative source for
`provider-battery-sysfs` hosts with multi-battery or vendor-threshold setups.
No model, wire, or presentation surface changes; desktops without UPower
reduce to `BatteryState::absent()`. Further D-Bus providers (network, MPRIS)
belong in this crate once the model grows their semantic state.

## Wire-protocol property tests

The internally tagged `ActionRequest` JSON contract is now pinned by tests
that round-trip every variant across boundary payloads and reject unknown
tags, missing fields, wrong types, and wrong tag casing. Bridges can rely on
rejected-not-defaulted behavior.

## Adoption map

| Consumer family | 0.6 change |
|---|---|
| `*_wgpu_bar` (4) | replace local `Gpu` with `WgpuPresenter`; pass `CpuFrame::damage` |
| `*_softbuffer_bar` (2) | `present_with_damage` from `CpuFrame::damage` |
| native bars (10) | `config-toml` feature + `BarConfig::load_default` |
| toolkit / Tauri bars | tag repin only |
