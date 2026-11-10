# Controlled input-to-visible-response benchmark

This opt-in, ignored test measures the existing production remote-control path:
owned native Wayland viewer input → authenticated loopback TCP → the authorized
application recipient → a sample-specific visual marker → actual source capture,
encoding and decoding → viewer GL readback → matching presentation feedback observed
by the viewer. It changes no wire protocol, permissions, lease duration or production
performance settings. The fixture uses its existing 30 FPS configuration.

## What this evidence means

Both Wayland displays and the application belong to the fixture. Rendering runs with
software GL requested (`EGL_PLATFORM=surfaceless`, `LIBGL_ALWAYS_SOFTWARE=1`,
`JWM_REQUIRE_HEADLESS_GL=1`). The report records requested environment settings;
it does not independently identify the actual renderer driver.

All timestamps use one process's monotonic clock. This is **not** physical
input-to-photon latency, DRM/page-flip acceptance, a real desktop interaction, a
wide-area network test or a two-machine benchmark. The fixture emits presentation
feedback only after checking actual GL pixels, then observes the exact matching
presented frame sequence. A unique run/sample marker prevents an old screen image
or a merely newer unrelated frame from satisfying a sample.

There are 2 warmup samples and 16 measured samples. The 25-second overall work budget
starts before GL and harness setup and is checked at progress boundaries. Blocking
fixture operations and teardown have their own bounded waits, so 25 seconds is not
a strict wall-clock process cap. This is a correctness/liveness budget, not a cloud
performance target. Setup and sample failures fail the test. No percentile or speed
threshold gates CI, and no `continue-on-error` hides failures. Scheduling, software
rendering, debug builds and CI contention affect these observations.

## Run explicitly

Use an environment authorized to create private Wayland sockets and initialize GL.
No user desktop or existing display is needed. From the repository root, after
installing the normal native development dependencies:

```sh
export EGL_PLATFORM=surfaceless
export LIBGL_ALWAYS_SOFTWARE=1
export JWM_REQUIRE_HEADLESS_GL=1
export XDG_RUNTIME_DIR="$(mktemp -d)"
export JWM_REMOTE_LATENCY_REPORT="$(mktemp -d)/remote-input-response.json"
env -u DISPLAY -u WAYLAND_DISPLAY cargo test --locked --no-default-features \
  --features remote-wayland,backend-wayland-udev --lib \
  remote::host::wayland_viewer_integration::host_control_integration::latency_benchmark::controlled_input_to_unique_visible_marker_latency \
  -- --ignored --exact --nocapture --test-threads=1
```

The report parent directory must exist. Ordinary test runs do not execute this
ignored benchmark. `.github/workflows/remote-wayland-safety.yml` explicitly runs it,
validates a schema-1 successful report with exactly 2 warmup and 16 measured samples,
all successful, and preserves the test's exit status through `pipefail`. Missing,
empty or oversized reports fail the step even if the test process returned success.
A three-minute CI step timeout bounds a stuck process outside the fixture's own
25-second checked work budget; neither timeout is a claimed response-time result.

## Report and interpretation

Schema version 1 retains warmup and measured sample records, including pending or
failed samples rather than silently dropping them. Each record contains a run token,
marker, previous/presented frame sequence and, when reached, these milliseconds:

- `inject_to_recipient_observed_ms`: local injection to observing the real target input.
- `recipient_observed_to_app_commit_ms`: application response/commit preparation.
- `app_commit_to_gl_verified_ms`: capture, transport and rendering until marker verification.
- `gl_verified_to_present_observed_ms`: verified pixels to observing matching presentation.
- `inject_to_present_observed_ms`: the complete controlled path.

These are observational stage boundaries, not isolated GPU, codec or network service
times. Quantiles report `n`, `min_ms`, `p50_ms`, `p95_ms`, `max_ms` using nearest-rank selection;
with only 16 samples, p95 is not a stable tail estimate. Inspect sample status and
counts before comparing distributions. Warmup is retained
for diagnosis but excluded from measured summaries. Do not replace failed samples
with zero or claim a complete 16-sample run from a partial report.

The report guard attempts to save JSON and bounded diagnostic data even when a
sample panics, without swallowing the failure. Forced termination, runner loss or
failure before guard construction can leave no report; missing evidence is not a
successful benchmark. CI uploads only the report and the last 1 MiB of this step's
log using `always()`, with three-day retention. It does not upload arbitrary display
state, screenshots, workspace files or unrelated logs.

This document specifies the measurement contract, not a measured latency claim.
Use the exact CI commit, JSON samples and build/runner context when publishing
results. No physical-display or production-network conclusion follows from these
controlled observations alone.

## Versioned observations and rejected experiment

[The 2026-11-10 evidence](benchmarks/remote-input-response-2026-11-10/README.md)
retains complete initial reports and a same-process ABBA experiment. Avoiding
retained-proof capture-slot accounting with a 20 ms wait regressed the controlled
workload, so that production change was reverted. The exact experimental commit
remains linked for reproduction; it is not a shipped optimization or a general
performance claim.

## Input-queue wake experiment

A separate opt-in test,
`latency_benchmark::input_wake_benchmark::controlled_input_wake_abba_latency`,
compares native input notification disabled/enabled in A/B/B/A order within one
process and binary. Four fresh sessions each run 2 warmups and 16 measured
responses. Capture-slot accounting, FPS, scopes, permissions, leases, encoding
and workload stay unchanged. The rejected retained-slot experiment is not used.

Only the test's owned host thread can disable the notification path for the
baseline. Production has no user-facing toggle. Per-session counts record notify
attempts/coalescing, drains and activity waits. These cover setup, warmup and
cleanup too; they are not input-action counts or CPU utilization. Generous count
bounds catch runaway loops, not hardware performance regressions. No latency
threshold suppresses negative results. Reports retain all 72 samples and both
order-block differences; a failed session prevents a successful comparison.

The mechanism wakes only the host's idle activity wait after authenticated native
input is queued or revoked. Native protocol sync, capture-ready and acceptance
waits remain uninterruptible by this hint. The actual queue, lease, source scope,
sequence, fresh-frame and FPS checks remain authoritative. A signal is never an
input event or an authorization grant. The benefit is limited to input arriving
while the host is in that idle wait, not arbitrary capture/codec stalls.

[Versioned input-wake observations](benchmarks/remote-input-wake-2026-11-10/README.md)
include complete same-process ABBA data, its notification/drain overhead and
limitations. They are not a comparison against a different CI run.
