# Native X11 alpha in Masonry/Vello

Source: Xilem commit `78d4f4b6878a1c2e2884edd72d7ecd83f1f6f704`,
`masonry_winit` 0.4.0, Apache-2.0. `UPSTREAM-SHA256.json` records original
source hashes, `Cargo.toml.orig` the original manifest, and `LICENSE` its license.
The normalized manifest retains the workspace's dependency settings and adds
safe X11 queries. Its Git dependencies and the bar's Xilem dependencies all
pin the exact upstream commit above, so fresh builds select the same compatible
API even when no generated lockfile is present.

The source patch is limited to `src/vello_util.rs`. Before creating the surface,
it verifies the actual Xlib/Xcb drawable on the application's active DISPLAY:
depth 32, matching visual, RGB888 masks and a live compositing manager. Only for
GLES with this native ARGB proof and an Opaque capability does the fallback
blitter use the existing premultiplication blend state. Vello's scene and
intermediate texture, the WSI alpha enum, and other platform/backend paths remain
unchanged. No new unsafe operation or public API is introduced.

Unavailable native evidence follows the original fallback. Native Wayland still
uses the advertised surface alpha modes. This patch does not claim physical GPU
or physical-display validation.
