# Output layout and framebuffer envelope

JWM's Wayland DRM backend composites into one **global framebuffer** whose size
is the axis-aligned bounding box of every enabled output's origin plus mode
(`proposed_output_framebuffer_size` / `KmsState::total_screen_size`).

## What still works without growing the envelope

Rearranging or disabling outputs is fine when the new layout's bounding box is
**unchanged** (same width and height). Soft-disable, DPMS, and same-size
modesets go through the normal output-management Apply path.

Hotplug that rebuilds the KMS session (`maybe_reinit_kms` → compositor
recreation) also allocates a fresh compositor at the new size.

## What is refused

A live `wlr-output-management` Test/Apply that would **grow or shrink** that
global envelope is refused up front with:

> runtime framebuffer envelope change … is not yet supported; reinitialize KMS
> to apply this layout

Sites: `output_management.rs` (Test) and `backend.rs` (Apply preflight).
`wlr_output_mgmt_allow_modeset` does **not** lift this guard.

## Why

Compositor `resize()` can recreate GLES targets, and KMS Apply can modeset, but
those are not yet one atomic transaction. Succeeding modeset then failing
resize (or the reverse) leaves KMS and the compositor disagreeing on size —
worse than asking the user to reinitialize.

## Workarounds

1. Prefer layouts that keep the same bounding box.
2. For a true size change: restart the session, or trigger a full KMS reinit
   (e.g. seat/VT cycle that rebuilds outputs), then re-apply the layout.
3. Tools like kanshi that expand the envelope need that restart/reinit step.

Tracked as gap 6 in [sota-gap-queue](sota-gap-queue.md). A real rebuild path
is multi-session work (DRM + GLES + capture/recording sizes in one rollback).
