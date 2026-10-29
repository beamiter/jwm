# gpui_bar

The GPUI example status bar uses JWM's shared `xbar_core` runtime and sidebar
projection, with native X11 and Wayland backends.

## Standalone GPUI dependencies

This bar consumes the published `gpui-pre = 0.3.6` and
`gpui-pre-platform = 0.3.6` packages from crates.io. They package GPUI and its
supporting crates from Zed revision
`bcf6582ce3500df93a8a39366640173e6786cea6`; Cargo does not clone or update the
Zed editor repository to build this bar. The Rust import names remain `gpui`
and `gpui_platform` through dependency aliases.

`gpui_platform` remains necessary: the core supplies the UI model, while the
platform package supplies native windows, input and rendering. Both `wayland`
and `x11` stay enabled. The package versions match
[`gpui_component_bar`](../gpui_component_bar/README.md), which also consumes
the shared sidebar, without adding `gpui-component` to this bar.

The startup transparency probe uses the same dependency lines as this GPUI
snapshot: `wgpu 29.0.4`, `x11rb 0.13.2` and `pollster 0.4`. Keep those aligned
when upgrading GPUI to avoid a second graphics stack and differing surface
capability APIs. The old macOS placeholder crates are no longer needed;
platform-specific dependencies resolve from their published packages.

## Build and verify

From the repository root, with the Linux GUI development libraries installed:

```bash
cargo check --manifest-path bars/gpui_bar/Cargo.toml --all-targets
cargo build --manifest-path bars/gpui_bar/Cargo.toml --release
```

An existing local `Cargo.lock` is refreshed by the first command. This example
bar keeps its lockfile local, so use an unlocked check after changing the
manifest before resuming `--locked` builds. Clearing the shared Cargo cache is
unnecessary. The first build still downloads GPUI's registry dependencies;
subsequent builds reuse Cargo's cache.

Inspect the resolved dependency graph with:

```bash
cargo metadata --manifest-path bars/gpui_bar/Cargo.toml --format-version 1
cargo tree --manifest-path bars/gpui_bar/Cargo.toml --duplicates
```

The native-bars CI workflow checks formatting, compiles all targets and rejects
any resolved dependency sourced from the Zed monorepo. It checks both GPUI
bars. Upgrade the pinned framework/platform pair together with the component
bar's compatible toolkit version, then verify both graphs.

Graph and compile checks do not exercise a real display. X11/Wayland startup,
text, four-edge resizing, input and transparent-background behavior still need
their normal graphical smoke checks when updating the toolkit.
