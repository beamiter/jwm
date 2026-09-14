# SOTA gap queue (daily-drive Wayland)

Ordered by how much each item hurts "open the laptop and work". This queue does
**not** authorize new layouts, bars, or effects until the first release gate in
[hardware-validation](hardware-validation.md) is closed.

## Status legend

- **done** — shipped in tree
- **partial** — usable; remaining work listed
- **blocked** — waiting on upstream or deliberate honesty
- **open** — next coding slices

| Priority | Gap | Status | Notes |
| --- | --- | --- | --- |
| 1 | HDR / color external elements | **partial** | Cursor/DnD/layer-top/overlay internalization is in tree. **EdgeGlow** is now `CommonLinearAware`. Remaining: tab bar, particles, postprocess, toast/OSD/system UI, …. Session lock stays external on purpose. See [hdr](hdr.md). |
| 2 | Native Wayland clipboard images | **done** | `Backend::set_clipboard_png` / data-device offer is primary; `wl-copy` is last-resort only. |
| 3 | XWayland interactive move/resize | **done** | `XwmHandler::{move,resize}_request` emit `MoveResizeRequest` into the shared Jwm drag pipeline. |
| 4 | Idle dim covers JWM overlays | **done** | Final fullscreen brightness after toast/OSD/system UI; mid-frame `u_brightness = 1.0`. Capture bake-in is a follow-up. See [idle](idle.md). |
| 5 | Async tearing (`PAGE_FLIP_ASYNC`) | **blocked** | Honest report via `submission_cannot_request_async_flip`; needs Smithay `queue_frame` support. Do not fake success. |
| 6 | Runtime framebuffer envelope change | **open** | Multi-output topology that changes the global FB size still asks for KMS reinit. |

## Next coding slices

1. Migrate another `LinearTarget` `EncodedOnly` class (particles, or tab bar if frosted backdrop is ready) to `CommonLinearAware` with a headless pixel oracle.
2. Bake final brightness into the Wayland capture view so screenshots/recordings match the dimmed session.
3. Framebuffer envelope rebuild path for hotplug growth/shrink.

Architecture debt that keeps this queue affordable: capability-split `Backend`,
feature state out of `Jwm`, and `compositor_common` consolidation — see
[architecture](architecture.md).
