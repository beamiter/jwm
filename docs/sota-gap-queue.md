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
| 1 | HDR / color external elements | **partial** | Cursor/DnD/layer internalization done. **WorkspaceTransition** (encoded snapshot decoded in-shader), **EdgeGlow**, **Particles**, and **TabBar** (frosted glass domain-aware) are `CommonLinearAware`. Remaining encoded chrome: postprocess, toast/OSD/system UI, …. Session lock stays external on purpose. See [hdr](hdr.md). |
| 2 | Native Wayland clipboard images | **done** | `Backend::set_clipboard_png` / data-device offer is primary; `wl-copy` is last-resort only. |
| 3 | XWayland interactive move/resize | **done** | `XwmHandler::{move,resize}_request` emit `MoveResizeRequest` into the shared Jwm drag pipeline. |
| 4 | Idle dim covers JWM overlays + capture | **done** | Final fullscreen brightness after toast/OSD/system UI; dedicated capture view bakes brightness; EncodedOutput screenshots read after the final pass. See [idle](idle.md). |
| 5 | Async tearing (`PAGE_FLIP_ASYNC`) | **blocked** | Honest report via `submission_cannot_request_async_flip`; needs Smithay `queue_frame` support. Do not fake success. See [compatibility](compatibility.md). |
| 6 | Runtime framebuffer envelope change | **documented** | Grow/shrink of the global FB bbox is refused by design until DRM+GLES are one transaction. Workarounds and sites: [output-layout](output-layout.md). |

## Next coding slices

1. Migrate the remaining `EncodedOnly` LinearTarget class (postprocess) or PostDelivery chrome (toast/OSD/system UI) when useful for HDR latch stability.
2. Upstream or vendor a Smithay path for `PAGE_FLIP_ASYNC` tearing (keep reporting honest until then).
3. Atomic DRM modeset + GLES resize transaction for envelope changes (multi-session).

Architecture debt that keeps this queue affordable: capability-split `Backend`,
feature state out of `Jwm`, and `compositor_common` consolidation — see
[architecture](architecture.md).
