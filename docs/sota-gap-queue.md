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
| 1 | HDR / color external elements | **done (software)** | All JWM chrome classes, including annotation, screenshot toolbar and recording cues, are common-linear-aware. Local-only cues use post-capture FP16 scratch delivery. Maintained Smithay frame properties bind framebuffer and the complete color chain in one TEST_ONLY/real request; matching-generation vblank alone publishes active output descriptions. Pending resources are bounded. Physical HDR acceptance remains unverified; async tearing remains separately blocked. See [hdr](hdr.md) and [frame transactions](wayland/HDR_FRAME_TRANSACTIONS.md). |
| 2 | Native Wayland clipboard images | **done** | `Backend::set_clipboard_png` / data-device offer is primary; `wl-copy` is last-resort only. |
| 3 | XWayland interactive move/resize | **done** | `XwmHandler::{move,resize}_request` emit `MoveResizeRequest` into the shared Jwm drag pipeline. |
| 4 | Idle dim covers JWM overlays + capture | **done** | Final fullscreen brightness after toast/OSD/system UI; dedicated capture view bakes brightness; EncodedOutput screenshots read after the final pass. See [idle](idle.md). |
| 5 | Async tearing (`PAGE_FLIP_ASYNC`) | **blocked** | Honest report via `submission_cannot_request_async_flip`; needs Smithay `queue_frame` support. Do not fake success. See [compatibility](compatibility.md). |
| 6 | Runtime framebuffer envelope change | **partial** | Default refusal remains; explicit `wlr_output_mgmt_allow_modeset` now enables staged GLES prepare / DRM transaction / publish, with reverse rollback on failure. Offscreen resource tests do not replace physical modeset/capture/VT stress. See [output-layout](output-layout.md). |
| 7 | Selected-output DVR and companion native completion | **done (software)** | Native Wayland selects up to four independent bounded v1 SDR endpoints with per-output extent/placement and hotplug reconnect. KMS monotonic page-flip and GLX observed OML SBC supply exact-token `Presented`/all-output `Retired` evidence. EGL/nested retain `None`; HDR/dma-buf payloads and pane-local anchors remain unsupported. Physical-panel certification was not performed or claimed. See [companion bridge](eva-companion-bridge.md), `artifacts/gap-completion-20261103/visual/validation.json`. |

Known limits of the linear tail (not regressions against the old
exact-sRGB fallback, which clipped the same way): while filters run, the
postprocess and blur chain use FP16 whenever the scene-linear target is live,
and the linear-domain glass backdrop cache prefers FP16 (falling back to the
encoded formats only if allocation fails). Unsupported external elements or output
geometry can still require a frame-global HDR fallback.

## Next coding slices

1. Keep framebuffer/color generation and lock/capture boundaries covered when adding any new chrome class; all current classes are common-linear-aware.
2. Upstream or vendor a Smithay path for `PAGE_FLIP_ASYNC` tearing (keep reporting honest until then).
3. Atomic DRM modeset + GLES resize transaction for envelope changes (multi-session).

Architecture debt that keeps this queue affordable: capability-split `Backend`,
feature state out of `Jwm`, and `compositor_common` consolidation — see
[architecture](architecture.md).
