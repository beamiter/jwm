# Native X11 alpha in Masonry/Vello

Source: Xilem commit `78d4f4b6878a1c2e2884edd72d7ecd83f1f6f704`,
`masonry_winit` 0.4.0, Apache-2.0. `UPSTREAM-SHA256.json` records original
source hashes, `Cargo.toml.orig` the original manifest, and `LICENSE` its license.
The normalized manifest retains the workspace's dependency settings and adds
safe X11 queries. Its Git dependencies and the bar's Xilem dependencies all
pin the exact upstream commit above, so fresh builds select the same compatible
API even when no generated lockfile is present.

The source patch touches `src/vello_util.rs` and `src/event_loop_runner.rs`.
Before creating the surface, it verifies the actual Xlib/Xcb drawable on the
application's active DISPLAY:
depth 32, matching visual, RGB888 masks and a live compositing manager. For
GLES or Vulkan with this native ARGB proof and an Opaque capability, the fallback
blitter uses the existing premultiplication blend state. The bar additionally
requires a real presented Vulkan pixel to retain half alpha before creating
its translucent window: it renders into a 2×2 off-screen probe window and reads
the redirected XComposite pixmap. An opaque-only Vulkan driver that discards
alpha still gets the solid fallback (or a separately verified GLES backend).
Vello's scene and intermediate texture, the WSI alpha enum, and other platform/backend paths remain
unchanged. No new unsafe operation is introduced.

`MasonryState::set_wgpu_backends` lets the bar select a probed backend from
`AppDriver::on_start`, before creating any surface/device. This avoids changing
process environment variables after logging/runtime threads have started. The
bar uses Masonry's WGPU major and adapter-selection helper, tries GLES only if
the default backend cannot carry alpha, and preserves explicit `WGPU_BACKEND`
choices. The setter refuses to replace an instance after a device exists.

Unavailable native evidence follows the original fallback. Native Wayland still
uses the advertised surface alpha modes. The pixel probe verifies the X11
pixmap on the active driver; it does not assume all drivers retain native alpha.
