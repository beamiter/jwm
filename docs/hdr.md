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
jwm-tool msg set_hdr_metadata --args '{"output": "HDMI-A-1", "enabled": true}'
jwm-tool msg set_hdr_metadata --args '{"output": "HDMI-A-1", "enabled": false}'
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
then configuration, then participation, then this frame's content. Everything
above `output_not_participating` is permanent: it fails the enable command,
and it drops a request the reconciliation is already carrying. Everything from
`output_not_participating` down is momentary, and the latch carries the
request across it. Participation sits on the boundary deliberately: testing it
first (as it was before) let a soft-disabled SDR panel latch a request nothing
could ever honour, answer `Ok` to `set_hdr_metadata`, and count towards
`hdr_signalling_enable_available`.

| Reason | Meaning |
| --- | --- |
| `scanout_chain_*` | The 10-bit format / plane / CRTC-stage / connector-property chain is incomplete. Five variants name which link; a sixth, `scanout_chain_cross_device`, exists for a multi-GPU delivery path that does not exist yet and is never reported today. |
| `edid_lacks_hdr_profile` | The panel's EDID advertises neither PQ nor HLG, so there is no CTA-861.3 blob that would describe it. |
| `connector_commit_rejected` | The driver refused the last signalling commit. Nothing changed on the hardware, so no frame is held; the request is dropped instead of retried, and a fresh `set_hdr_metadata … enabled: true` is the one way to ask again. Only an answer the driver actually gave counts: a commit that never reached one — `EACCES` because DRM master is held elsewhere across a VT switch, `EBUSY` racing a modeset, a device on its way out — leaves the request standing, and the next frame that owns the device asks again. |
| `advanced_color_management_disabled` | Clients are being told this output is exact sRGB. |
| `color_pipeline_offload_disabled` | `kms_color_pipeline_offload` or the scene-linear render path is off. |
| `software_region_topology_unsupported` | Some participating output is scaled, rotated, or placed at a negative origin. The single global framebuffer has no per-output scale or transform stage, so the region planner rejects the *whole* layout — and with it the software route HDR needs. Change the scale or transform; waiting does not help. |
| `output_not_participating` | DPMS off, or soft-disabled through wlr-output-management. |
| `scene_linear_target_inactive` | No FP16 common-linear target; the frame is composed in encoded sRGB. |
| `linear_tail_unsafe` | Something in the frame tail — a toast, a session lock, an unimportable cursor tree — is assembled outside the common-linear pass, so the frame falls back to exact sRGB. |
| `legacy_gamma_override_active` | A `zwlr-gamma-control` client owns this CRTC's ramp. |
| `color_delivery_blocked` | KMS colour state is unresolved and presentation is being held. |
| `overlapping_output_profile_conflict` | This output is mirrored onto another. Giving one of two cloned outputs a different transfer function makes the region plan fail, which would withdraw the signal the next frame and re-assert it the frame after — forever. |
| `hardware_lut_route_clips_hdr_headroom` | See below. |
| `no_software_delivery_region` | No software delivery region covers this output this frame, so nothing applies its transfer function. A layout the planner can *never* cover reports `software_region_topology_unsupported` above instead. |

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

Every frame reconciles the latched request against that frame's evidence.
Migrated JWM chrome, including annotations, the screenshot toolbar and
recording cues, no longer forces an encoded fallback. A remaining unsupported
scene or external element can still require withdrawal. The request is intent;
`hdr_metadata_active` is a confirmed presentation fact.

An assert or withdrawal stages the complete target color chain without
changing current scanout. Smithay associates it with the matching newly
rendered framebuffer: FB_ID, CRTC stages, Colorspace and HDR_OUTPUT_METADATA
share one TEST_ONLY request and the identical real atomic request. A failed
probe or real submission retains the previous framebuffer/color pairing.

Each exact queued framebuffer's accepted, timestamped vblank publishes its
observed output description. A newer desired generation does not suppress the
older framebuffer that actually reached scanout, and the observation never
overwrites the newer target. Its retry remains armed until that target presents.
Staging preserves the previous confirmed presentation and queue identity. Surfaces already on the output then receive
`wp_color_management_surface_feedback_v1.preferred_changed`, and bound output
objects receive `image_description_changed`. Disabling the advanced policy
changes the desired next frame; the confirmed HDR description persists until
the matching SDR withdrawal presents. Replaced blob IDs stay owned through
pending/cancelled frames and are released only after the newest desired generation presents;
the current/retired/new allocation budget is 64 IDs per output.

A permanent refusal drops the request instead of paying for an HDR software
route that cannot be delivered. Transient DRM ownership/resource errors retain
the request for retry. Dark outputs may drop their public active claim while
retaining connector state; the next participating framebuffer carries the
complete desired state, so no differently encoded framebuffer is presented
under inherited HDR properties. DPMS writes remain ordered before dark-output
teardown; a failed blank never strips the visible framebuffer's color chain.

A latched request also survives a rebuild of the KMS state. A VT switch back
and every connector hotplug construct fresh outputs; the request and any
latched VRR override (backend-internal; no IPC command sets it yet) are
carried across by output name, and a name that is gone says so in the log
rather than vanishing.

## What the reports mean

```sh
jwm-tool msg get_hdr_status
jwm-tool wayland-status --json | jq '.color_delivery, .render_decisions.hdr'
```

Everything HDR-related is reported from the *last successful presentation* —
a page-flip or vblank that actually landed — never from configuration and
never from an attempt. A failed commit does not overwrite the previous
success; a DPMS or disable/enable cycle invalidates the old evidence rather
than carrying it forward, and reports `null` until a replacement frame lands.

- `color_management.session_policy.hdr_active` — whether any output presented
  with HDR metadata.
- `…delivery_capabilities.hdr_signalling_enable_available` — whether the
  enable *command* would be accepted on at least one output. Not "nothing is
  refusing right now": a refusal the request latch carries across (a toast, a
  route change) does not fail the command, and reading one as unavailable
  would hide the feature on the hardware that supports it. An empty
  `hdr_enable_refusals` array means the backend has no gate of its own (X11,
  headless), which is **not** an availability claim.
- Per-output `color_policy.selected_transfer_function` / `selected_primaries`
  — the profile the display is being told *now*. Signalled outputs report
  their EDID-derived profile; everything else reports sRGB.
- `colorspace_signal` — `bt2020_rgb` while signalling, `default_sdr`
  otherwise. Both properties move in one request, so these cannot disagree.

## Limitations

- **Entering or leaving HDR may be a blocking modeset.** The frame-associated
  transition is tested and submitted with ALLOW_MODESET because some drivers
  require it for connector signaling. Stable color generations use an empty
  property vector and keep Smithay's ordinary page-flip path. A permanent
  combined-request rejection falls back to software/SDR policy; transient
  device errors remain retryable.
- **Not verified against a real HDR display.** Recorder fault tests establish
  request association and failure boundaries; surfaceless GLES pixel tests
  establish the software color/capture behavior. They do not certify a panel's
  interpretation of CTA-861.3 metadata or driver support for a physical chain.
- **Asynchronous tearing remains unsupported.** The maintained extension adds
  frame-bound color properties, not PAGE_FLIP_ASYNC.
- The local-only capture cues use an FP16 scratch copy and a second output
  delivery after capture. This preserves capture exclusion and keeps the
  persistent linear scene clean, at the cost of one extra copy/conversion on
  frames showing those cues.
- Non-D65 white points are Bradford-adapted, but there is no full chromatic
  adaptation transform selection.
- Direct scanout stays blocked while the scene-linear path is active, so an
  HDR client's buffer is composited rather than scanned out directly.
- The frame-tail verdict is frame-global: a cursor that fails to import on
  one output demotes the frame, and therefore every output's HDR signal.

## Where it lives

- `src/backend/udev_kms.rs` — `hdr_enable_refusal` (the policy),
  `hdr_signalling_action` and `hdr_withdrawal_reaches_connector` (the control
  loop), `hdr_scanout_chain_gap` (the 10-bit chain),
  `set_hdr_metadata_for_output` (staging), `frame_color_properties` (frame snapshot),
  `on_vblank` (confirmed publication),
  `carry_latched_intents` (surviving a rebuild).
- `src/backend/wayland_udev/color_management.rs` —
  `ColorManagerState::on_output_description_changed`, the client half of an
  assert or withdrawal.
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

`jwm-tool health` prints compact `hdr` for hdr operators.

See [the maintained Smithay frame transaction extension](wayland/HDR_FRAME_TRANSACTIONS.md)
for API ownership, generation, resource bounds and software validation details.
