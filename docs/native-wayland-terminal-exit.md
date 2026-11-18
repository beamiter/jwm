# Native nested terminal dismissal

The `term` scratchpad keeps a drawing-only exit copy in the nested Wayland
winit backend. Input geometry is parked immediately; hit-testing excludes its
surface and popups while the copy exists. A stationary pointer is retargeted
without waiting for mouse motion. Drawing promotes this copy above ordinary
applications but below system windows and the existing top/overlay/lock passes.

This is a narrow channel, not `has_compositor=true`. Native reveal and other
animations keep their existing paths. X11 and DRM/KMS use the default no-op hook
and retain their previous rendering. The shared state file lives under
`wayland_udev`, but only the winit backend opts into the new channel.

The previous native winit baseline is preserved in draft #114,
run 44404173276, artifact 14475330815. Its original failures are not rerun to
claim success. A follow-up corrected a diagnostic frame-count cap; no pixel
threshold was relaxed.

Acceptance uses real xdg-shell/wl_shm clients on a private Xvfb-hosted winit
compositor, with native keyboard/pointer event receipts and continuous host
output pixels. It tests floating/PiP/fullscreen overlap, cleanup, reversal and
stationary-pointer handoff during a visible ordinary-window exit. No user
computer/session is used.

Limits: wayland-x11 is compiled but cannot be runtime-accepted on Xvfb, whose
missing DRI3/DRM makes initialization fail. It remains on its original path with no new visual hook or rendering branch
enabled. Its host-specific behavior remains unverified. No claims about physical GPUs,
DRM/KMS, touch gestures, native Above requests, or real lock/OR pixel acceptance
are made. System-layer precedence has static/unit coverage, not a credentialed
lock-screen test. A first-frame submission is requested synchronously after input release, but
host swap/presentation may be asynchronous; this does not promise zero-frame
latency. All live acceptance remains pending until the exact draft
head's CI artifacts have been reviewed.
