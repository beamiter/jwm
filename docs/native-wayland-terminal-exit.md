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


## Continuous regression coverage

`wayland-terminal-acceptance.yml` runs on relevant pull requests (including
forks and drafts), pushes to `master`, and manual `workflow_dispatch` runs.
There is no branch-name gate. Both automatic events share path coverage for
production Rust, workspace/path dependencies, lockfiles/build configuration,
the native client, the sampler it imports, dependency installation, and this
regression's workflow/documentation. Unrelated prose and frontend-only changes
outside these paths do not start this job. Manual dispatch becomes available
once the workflow exists on the default branch.

Each matching event adds one Ubuntu runner job, bounded to 40 minutes, including
native release compilation, four focused tests and the same strict native
pixel/input test. The live test is bounded to five minutes; dependencies to ten.
A PR update and its eventual master merge intentionally each incur one job so
the merged tree is tested too. Rust caches reduce repeat build cost but are not
a guarantee. Pixels and native receipts are retained as artifacts even when the
behavior test fails; there is no continue-on-error or softened threshold. This
adds CI compute/artifact cost, not a claim that repository branch protection has
been changed. The unsupported wayland-x11 runtime remains excluded.
