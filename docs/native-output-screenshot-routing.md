# Native output-region screenshot routing

The native DRM/KMS screenshot entry point accepts desktop-global region
coordinates. Its existing offscreen renderer captures one output's element
list in output-local pixels. A global x coordinate must not be clipped directly
against that local framebuffer width.

## Supported extension

A region wholly inside a nonprimary output is routed to that output when the
entire current layout has Normal transform, scale exactly 1, nonnegative origins,
positive known mode sizes, no output overlaps, and representable global extents.
The route subtracts both output origin coordinates before cropping. Gaps between
outputs are permitted; a request crossing a gap or output boundary is not routed.
The target output and requested image must fit the existing RGBA allocation limit.
Output snapshot count is capped at 256.

Primary-output requests, full screenshots, spanning requests, negative-origin
layouts, overlapping outputs, rotation and non-unit scaling retain their existing
path. This change does not make those existing unsupported multi-output cases
correct. It does not alter X11 or nested backends.

The route stores Smithay Output identity and the complete geometry/transform
eligibility snapshot. A topology change fails the pending request instead of
recropping a different output. DPMS-off, disabled outputs, cancellation and a
three-second presentation deadline also retire it. Deadline maintenance runs
outside presentation/session gates and contributes to the event-loop wakeup.
KMS teardown explicitly drops routed requests even if other references survive.
The existing screenshot permit remains owned through queue, readback and PNG
worker, so this path shares the process-wide admission budget. Failed requests
disconnect memory recipients and release that permit; file failures are logged.

## Pixel contract and validation boundary

The per-output element list already offsets the common compositor texture by
negative output origin. The existing offscreen capture renderer uses Normal
transform and scale 1. This route reuses that renderer only where those values
match output configuration. It does not apply another rotation or row flip.
The compositor texture's existing Flipped180 handling is a texture/readback
orientation detail, not evidence for monitor rotation support.

Before consuming a route, the current compositor framebuffer must cover the
output rectangle. This rejects stale smaller textures after a failed resize.
The bounded CPU crop copies exact rows from the mapped output image, and is used
by the production region readback, not just by tests.

CPU tests exercise actual route selection and patterned RGBA copy at horizontal,
vertical and offset secondary origins; boundary/unsupported-layout fallback;
identity/reorder/geometry changes; disabled output, cancellation and deadline;
queue ownership/teardown; and framebuffer/copy bounds. Source-wiring assertions
cover maintenance, wakeup and teardown hooks. They do not execute DRM or GLES.
A native build and a patterned two-output hardware screenshot remain necessary
before claiming end-to-end validation. Rotation/scaling require an explicit
oriented-buffer contract and hardware checks before broadening this route.
Full-desktop composition additionally needs bounded destination allocation,
intersection placement, gap fill and deterministic overlap policy; the separate
experimental capture planner is not production composition.

## Failure reporting and CI

The backend's `Ok(true)` acknowledges queue admission, not successful PNG output.
For memory capture, retiring a route disconnects the sender; the receiver returns
an error, and the existing completion handler produces a `Screenshot failed`
toast. For file capture, the route logs its specific failure, while the existing
file watcher reports a generic failed toast after its 30-second deadline if no
file appears. The low-level file API has no asynchronous completion receipt.
The three-second route deadline therefore bounds queued capture ownership, not
the file destination's user-visible failure delay. Inactive-session toasts become
visible when the normal UI loop can present again.

Existing CI's default `cargo check --locked --all-targets` and
`cargo test --locked --lib --bins --tests` include `backend-wayland-udev`, so they
compile this production integration and execute its unit tests. The feature
matrix also checks that backend with default features disabled. Targeted commands
for a provisioned Linux build environment are:

```sh
cargo check --locked --all-targets --no-default-features --features backend-wayland-udev
cargo test --locked --lib screenshot_route::tests
cargo test --locked --lib routed_request_keeps_admission_until_teardown_and_disconnects_receiver
```

The last test exercises the actual screenshot admission permit and memory channel,
not the generic payload stand-in used by the standalone queue tests. These tests
need no screenshot device or display; they do not replace a GPU integration test.
