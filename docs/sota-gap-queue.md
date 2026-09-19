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
| 1 | HDR / color external elements | **partial** | Cursor/DnD/layer internalization done. **WorkspaceTransition** (encoded snapshot decoded in-shader), **Postprocess** (filters run on an encoded copy, result decoded; cursor stays above it on every route), **Toast**, **OSD** and **DebugHud** (drawn at section 18a ahead of delivery; glass, fills and UI text honor the bound domain), **EdgeGlow**, **Particles**, and **TabBar** (frosted glass domain-aware) are `CommonLinearAware`. Remaining encoded chrome (all post-delivery): system UI, annotation, screenshot toolbar, recording overlay. Session lock stays external on purpose. See [hdr](hdr.md). |
| 2 | Native Wayland clipboard images | **done** | `Backend::set_clipboard_png` / data-device offer is primary; `wl-copy` is last-resort only. |
| 3 | XWayland interactive move/resize | **done** | `XwmHandler::{move,resize}_request` emit `MoveResizeRequest` into the shared Jwm drag pipeline. |
| 4 | Idle dim covers JWM overlays + capture | **done** | Final fullscreen brightness after toast/OSD/system UI; dedicated capture view bakes brightness; EncodedOutput screenshots read after the final pass. See [idle](idle.md). |
| 5 | Async tearing (`PAGE_FLIP_ASYNC`) | **blocked** | Honest report via `submission_cannot_request_async_flip`; needs Smithay `queue_frame` support. Do not fake success. See [compatibility](compatibility.md). |
| 6 | Runtime framebuffer envelope change | **documented** | Grow/shrink of the global FB bbox is refused by design until DRM+GLES are one transaction. Workarounds and sites: [output-layout](output-layout.md). |

Known limits of the linear tail (not regressions against the old
exact-sRGB fallback, which clipped the same way): the postprocess filters
round-trip through an 8-bit encoded copy, so while a filter is on the frame
keeps no headroom above SDR white; the glass backdrop cache for the linear
domain is 8-bit unless `hdr_enabled`, which can band under dark wallpapers.
An FP16 postprocess copy and glass cache would lift both.

## Next coding slices

1. Every LinearTarget class, toasts and the OSD are common-linear-aware. Next: system UI (launcher, prompts, tags grid, control center — many programs; the lock shield must stay ahead of the capture view).
2. Upstream or vendor a Smithay path for `PAGE_FLIP_ASYNC` tearing (keep reporting honest until then).
3. Atomic DRM modeset + GLES resize transaction for envelope changes (multi-session).

Architecture debt that keeps this queue affordable: capability-split `Backend`,
feature state out of `Jwm`, and `compositor_common` consolidation — see
[architecture](architecture.md).
