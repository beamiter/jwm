# HDR and colour delivery

JWM's Wayland/KMS backend composites in a normalized linear-sRGB working
space and converts to each output's own profile at the end of the frame. HDR
signalling — telling a display, through the kernel's `Colorspace` and
`HDR_OUTPUT_METADATA` connector properties, that it is receiving PQ/BT.2020 —
is the last step of that pipeline and the one with the sharpest failure mode:
signalling HDR over a frame the compositor encoded as sRGB is a whole-screen
colorimetric error, not a subtle one.

So it is off unless asked for, refused when it cannot be honoured, and
withdrawn the instant the frame stops qualifying.

## Asking for it

```sh
jwm-msg '{"command": "set_hdr_metadata", "args": {"output": "HDMI-A-1", "enabled": true}}'
jwm-msg '{"command": "set_hdr_metadata", "args": {"output": "HDMI-A-1", "enabled": false}}'
```

`enabled: true` latches *intent* on that output. It does not commit anything
by itself: the actual `Colorspace` + `HDR_OUTPUT_METADATA` request is made,
and unmade, by the per-frame reconciliation, because every precondition below
can change without a modeset.

The command fails immediately only when the refusal is one that waiting
cannot fix — an SDR panel, an incomplete scanout chain, a switch that is off.
A refusal that is merely momentary (a toast on screen right now) does not
fail the request; the latch is exactly what carries it across.

The signal follows the *content* profile too: `advanced_color_management` must
be on, because with it off clients are told the output is exact sRGB, and
signalling HDR while advertising sRGB is the same lie in the other direction.
It is gated by `JWM_COLOR_MANAGEMENT_ADVANCED=1`.

## Why it might not be on

Each output reports the first reason it has, in a fixed order — hardware,
then configuration, then this frame's content:

| Reason | Meaning |
| --- | --- |
| `output_not_participating` | DPMS off, or soft-disabled through wlr-output-management. |
| `scanout_chain_*` | The 10-bit format / plane / CRTC-stage / connector-property chain is incomplete. Six distinct variants name which link. |
| `edid_lacks_hdr_profile` | The panel's EDID advertises neither PQ nor HLG, so there is no CTA-861.3 blob that would describe it. |
| `advanced_color_management_disabled` | Clients are being told this output is exact sRGB. |
| `scene_linear_target_inactive` | No FP16 common-linear target; the frame is composed in encoded sRGB. |
| `color_pipeline_offload_disabled` | `kms_color_pipeline_offload` or the scene-linear render path is off. |
| `linear_tail_unsafe` | Something in the frame tail — a toast, a session lock, an unimportable cursor tree — is assembled outside the common-linear pass, so the frame falls back to exact sRGB. |
| `legacy_gamma_override_active` | A `zwlr-gamma-control` client owns this CRTC's ramp. |
| `color_delivery_blocked` | KMS colour state is unresolved and presentation is being held. |
| `hardware_lut_route_clips_hdr_headroom` | See below. |
| `no_software_delivery_region` | No software delivery region covers this output, so nothing applies its transfer function. |

```sh
jwm-tool wayland-status --json | jq '.color_management.session_policy.hdr_enable_refusals'
```

### An HDR request steers the delivery route

When every participating CRTC owns the gamut matrix and OETF (the
`kms_ctm_gamma_lut` route), the compositor writes working-linear values into
an RGB10_A2 or RGBA8 output framebuffer. Those are unorm formats, so
everything above reference white is clipped *before* the CRTC's GAMMA_LUT
sees it — and content above reference white is precisely what HDR exists for.
The LUT is also keyed by transfer function alone, so no peak-dependent
tone-map can be baked into it.

The software route applies the OETF in the encode shader *before* that write,
so PQ-encoded values land inside [0,1] with their headroom intact. HDR
therefore requires `software_per_output_regions`, and a request steers the
route: while any participating output has one, the CRTC pair is suppressed for
the whole delivery group (the pair is all-or-nothing). That is the visible
cost of enabling HDR — and it is not optional, because the hardware pair is
preferred wherever the CRTC has the stages, so without the steering an enable
would be refused forever on exactly the hardware capable of HDR.

`hardware_lut_route_clips_hdr_headroom` therefore stays as a backstop rather
than the normal answer. Removing the trade-off entirely needs an FP16 scanout
framebuffer and a wider LUT key.

## Withdrawal is automatic

Every frame reconciles the latched request against that frame's evidence, so
a toast appearing takes the signal off and the toast clearing puts it back —
without the user touching anything. That is why the request is stored
separately from the state: `hdr_metadata_active` is a post-commit fact, and
with no latch the first toast would drop HDR permanently.

A withdrawal clears `Colorspace` to Default and the metadata blob to zero in
the same controlled atomic request that set them, so the sink is never left
told BT.2020 with no metadata behind it.

The one exception is an output that has gone dark — DPMS off, or
soft-disabled through wlr-output-management. There the claim is dropped
without a commit: the properties belong to a display that is off, whether the
commit is even accepted is driver-dependent, and a failure would hold
presentation for every other output on the device. Nothing reports HDR active
on an output that is not presenting, and the signal is re-asserted with a
fresh commit when it comes back.

## What the reports mean

```sh
jwm-msg '{"query": "get_hdr_status"}'
jwm-tool wayland-status --json | jq '.color_delivery, .render_decisions.hdr'
```

Everything HDR-related is reported from the *last successful presentation* —
a page-flip or vblank that actually landed — never from configuration and
never from an attempt. A failed commit does not overwrite the previous
success; a DPMS or disable/enable cycle invalidates the old evidence rather
than carrying it forward, and reports `null` until a replacement frame lands.

- `color_management.session_policy.hdr_active` — whether any output presented
  with HDR metadata.
- `…delivery_capabilities.hdr_signalling_enable_available` — whether at least
  one output currently has no refusal. An empty `hdr_enable_refusals` array
  means the backend has no gate of its own (X11, headless), which is **not**
  an availability claim.
- Per-output `color_policy.selected_transfer_function` / `selected_primaries`
  — the profile the display is being told *now*. Signalled outputs report
  their EDID-derived profile; everything else reports sRGB.
- `colorspace_signal` — `bt2020_rgb` while signalling, `default_sdr`
  otherwise. Both properties move in one request, so these cannot disagree.

## Limitations

- **Not verified against a real HDR display.** The compositor is proven
  internally consistent — it never signals HDR over sRGB pixels, and the
  pixel maths is pinned by a surfaceless-EGL oracle at LSB tolerance — but no
  panel has confirmed that the 32-byte CTA-861.3 blob is interpreted as
  intended. The kernel accepts a syntactically valid blob without validating
  its meaning.
- The framebuffer and the colour properties are not committed in one ioctl.
  Smithay's `DrmCompositor` owns the framebuffer commit with no
  property-injection hook, so pairing is guaranteed by ordering (the colour
  request strictly precedes the frame's queue) plus a last-success
  invalidation clock, and the swapchain's bit depth is covered by the chain
  validation.
- Non-D65 white points are Bradford-adapted, but there is no full chromatic
  adaptation transform selection.
- Direct scanout stays blocked while the scene-linear path is active, so an
  HDR client's buffer is composited rather than scanned out directly.
- The frame-tail verdict is frame-global: a cursor that fails to import on
  one output demotes the frame, and therefore every output's HDR signal.

## Where it lives

- `src/backend/udev_kms.rs` — `hdr_enable_refusal` (the policy),
  `hdr_signalling_action` (the control loop), `hdr_scanout_chain_gap` (the
  10-bit chain), `set_hdr_metadata_for_output` (the atomic request).
- `src/backend/hdr_metadata.rs` — the CTA-861.3 static metadata blob.
- `src/backend/edid.rs`, `src/backend/color_policy.rs` — EDID HDR capabilities
  and the image description they imply.
- `src/backend/wayland_udev/color_pipeline.rs` — transfer functions, gamut
  matrices, tone-map policy, the delivery LUT.
- `src/jwm/ipc_handler.rs` — `get_hdr_status`, the colour session policy, and
  the render decisions.

## Related surfaces

- [compatibility.md](compatibility.md) — VRR, tearing, and the per-output
  presentation policy.
- [performance.md](performance.md) — direct scanout and the frame loop.
