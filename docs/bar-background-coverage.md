# Bar background coverage

All 28 bundled bars consume the same `behavior.blur_status_bar` preference
(default false/clear; explicit true/frosted), through the shared wire's reserved
optional-bool byte. This is policy coverage, not a claim of runtime validation
on 28 display stacks. Background alpha never changes overall window opacity.
An explicit per-bar `background_opacity` still controls its tint.

| Bars | Presentation and fallback |
| --- | --- |
| x11rb_bar, xcb_bar | Cairo: compositor + verified ARGB visual; otherwise solid |
| x11rb_glow_bar, xcb_glow_bar | Cairo/GL: existing compositor + ARGB gate; otherwise solid |
| x11rb_wgpu_bar, xcb_wgpu_bar | Cairo/WGPU: existing visual and surface-alpha gates; otherwise solid |
| winit_glow_bar | Cairo/GL: verified native Wayland or X11 ARGB/compositor; otherwise solid |
| winit_wgpu_bar, tao_wgpu_bar | Cairo/WGPU: native Wayland or X11 ARGB/compositor, additionally alpha-capable surface; otherwise solid |
| winit_pixels_bar, tao_pixels_bar | Cairo/Pixels: native Wayland premultiplied-alpha success, or existing X11 ARGB/compositor gate; otherwise solid |
| winit_softbuffer_bar, tao_softbuffer_bar | Cairo/software: existing X11 ARGB/compositor capability; native Wayland alpha is not asserted, solid fallback |
| tao_glow_bar | Official Cairo/GL: verified Wayland EGL alpha or X11 ARGB/compositor; otherwise sharp/frosted sampled wallpaper, then solid fallback |
| gtk_bar, relm_bar | Shared GTK adapter: existing compositor/RGBA-visual gate; otherwise solid |
| egui_bar, gpui_bar, iced_bar, xilem_bar, dioxus_bar | Live background preference through existing presenter capability gates; otherwise solid |
| gpui_component_bar | Existing renderer capability is explicitly false: safe solid background remains |
| tauri_leptos_bar, tauri_react_bar, tauri_solid_bar, tauri_svelte_bar, tauri_vue_bar, tauri_yew_bar | Shared Tauri bridge requests a transparent window, but does not claim verified native alpha. Sharp/frosted configured wallpaper fallback; no wallpaper means solid |

Wallpaper sampling cannot reveal live windows or an animated compositor layer.
Cairo, GTK, egui and Dioxus add contrasting text outlines for clear backgrounds;
other presenters retain their existing opaque controls/text treatment. Contrast
on arbitrary wallpaper still needs visual testing. Per-pixel clear mode does
not strip intentional selection/pill fills.

## Validation boundary

Standalone production-module/ extracted-source checks cover 16 glass/policy,
46 shared-wire, and 4 toolkit CSS/fallback tests. Wire remains version 14 and
3112 bytes. New config/default, reducer/reload and Cairo pixel tests are checked
in for normal Cargo execution. Local full Cargo builds cannot resolve the
uncached `drm` dependency offline; no new dependencies were downloaded.

`.github/workflows/native-bars.yml` checks shared wire and all core features,
14 native Cairo presenters, the toolkit presenters and six web frontends. Its
core job runs Clippy and tests; native presenter jobs compile/test their targets;
other toolkit/web jobs primarily compile and run existing startup checks.
The six Tauri Rust hosts are an explicit exception: CI only formats/checks
static wiring, because their existing unpinned upstream zune-jpeg dependency
failed resolution/build. Tauri shared bridge Rust changes therefore still need
a successful real host build before release; static tests are not compilation.
Main `.github/workflows/ci.yml` supplies JWM config/backend feature compilation.
These CI jobs must pass on the candidate before claiming build validation.
Neither CI nor the local CPU tests prove compositor/driver visuals, native
Wayland alpha, desktop blur or multi-monitor wallpaper sampling at runtime.
