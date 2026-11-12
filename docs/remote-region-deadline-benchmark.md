# Test-only region-deadline continuous-animation experiment

This experiment compares the production `New250` policy with the private,
`cfg(test)`-only `RegionDeadline` policy. It does not enable a production option,
change permissions, change the wire protocol, or relax the existing 500 ms
blink, popup, resize, pointer, and control gates. The older three-arm and
factorial benchmarks retain their original 16 markers and 500 ms schedule.

## Fixed workload and execution

The same owned Wayland source, authenticated loopback TCP transport, and actual
Wayland viewer run through Smithay GLES with software EGL/llvmpipe. There is no
physical monitor or hardware refresh rate in this fixture. Presentation is
controlled protocol feedback after actual GL readback, not physical-display or
network-WAN latency. The source is 800×600, the host remains at 12 FPS, and JPEG
quality is fixed at 60. All arms use identical source hashes, credit rules,
two-second per-tile stability, and the existing 65,536-pixel PNG batch bound.

Each session submits 80 distinct generations, at fixed 100 ms scheduled instants
from 0 through 7.9 seconds, without waiting for earlier presentations. An 8-bit
Gray identifier occupies exactly two 16×16 codec tiles (32×16 total). All 256
possible identifiers have an exhaustive production-JPEG round-trip test, with
uncertain-cell rejection. The remaining 479,488 pixels contain unchanged fine,
chromatic glyph-like strokes, so exactness requires multiple bounded PNG batches.
Every channel of every static pixel, including all neighboring edge pixels,
participates in the SSE comparison.

The active observation window ends at 8 seconds. A further 1 second drains
in-flight work, followed by bounded endpoint shutdown. A presentation observed
after 8 seconds is always cleanup-only, never active-window success. Four
sessions run in mirrored order: `New250`, `RegionDeadline`, `RegionDeadline`,
`New250`. The same test also retains endpoint failures and missing generations.

```sh
export EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 JWM_REQUIRE_HEADLESS_GL=1
export XDG_RUNTIME_DIR="$(mktemp -d)"
export JWM_REMOTE_REGION_DEADLINE_REPORT=/path/to/private/region-deadline.json
base=remote::host::wayland_viewer_integration::host_lossless_integration::scheduler_benchmark
scripts/test.sh --no-default-features \
  --features remote-wayland,backend-wayland-udev --lib \
  "$base::continuous_gray_markers_survive_production_jpeg_and_reject_uncertain_cells" \
  -- --exact --test-threads=1
scripts/test.sh --no-default-features \
  --features remote-wayland,backend-wayland-udev --lib \
  "$base::actual_static_lossless_region_deadline_continuous_measurement" \
  -- --ignored --exact --test-threads=1
python3 scripts/summarize-remote-region-deadline.py \
  "$JWM_REMOTE_REGION_DEADLINE_REPORT" > /path/to/private/summary.json
python3 scripts/summarize-remote-region-deadline.py --public-summary \
  "$JWM_REMOTE_REGION_DEADLINE_REPORT" > /path/to/aggregate-summary.json
```

The mandatory bounded CI step in `remote-wayland-safety.yml` verifies exact test
names and counts, runs the exhaustive identifier and static-region boundary
checks, requires headless GL, and verifies that the ignored experiment actually
ran once. The report environment variable supplies only its private output path;
it cannot select the production scheduler.

## Acceptance and interpretation

Both `New250` sessions must reproduce starvation: no exact static presentation
within 8 seconds, and an inexact static presentation in the final active second.
Both `RegionDeadline` sessions must reach static SSE=0 within 8 seconds after
more than one presented PNG batch, then present a generation whose source commit
occurred after that exact presentation and before the active window ends. This
proves exact static text while animation continues, rather than convergence only
after animation stops. No observed static exact→inexact regression is allowed,
including the drain and recorded shutdown presentation callbacks.

Dynamic-update performance is reported, not hidden behind the static gate. Every
planned generation stays in the denominator, including never-submitted, missing,
late (>100 ms), and cleanup-only presentations. Both scheduled→presented and
commit-issued→presented distributions include count, nearest-rank p50/p95, and
maximum. Missing samples are never assigned zero latency. Per-stage fixture
costs and source-commit gaps expose source scheduling or software-rendering load.
Compare all four sessions' p95, maximum, missing counts, and late counts before
claiming an improvement. Static convergence alone does not establish dynamic
non-regression or a hard real-time guarantee.

Encoded JPEG/PNG counts and timings, decoded/presented PNG counts, comparison
cost, stale-PNG drops, and source-envelope payload bytes are reported separately.
Payload excludes Noise/TCP overhead. Encode counters include discarded attempts.
Whole-session host counters include startup and shutdown; active-window counters
are the last snapshot at or before 8 seconds and still include common startup.
Neither is automatically a throughput figure. Raw records retain source identity,
source timestamps, GL/readback and feedback stages, and sequence-associated actual
presentation observations. Quantiles apply only to observed matching markers.

## Initial local validation

A sequential software-GL run on 2026-11-12 passed all four sessions. The two
RegionDeadline sessions reached exact static pixels at 3.877 and 3.858 seconds,
using eight presented PNG batches each. Later animation was presented and no
static exact-to-inexact regression occurred. Neither New250 control converged
within the eight-second active window.

Scheduled-to-present p95/max latency, in mirrored session order, was
354.94/515.16, 301.09/365.62, 265.31/323.98, and 237.71/252.61 ms. Missing active-window
generations out of 80 were 42, 24, 29, and 26. These mixed dynamic results do not
establish a latency improvement or dynamic non-regression: the final control had
lower p95/max latency than either candidate session. The policy therefore remains
test-only. A prior run under concurrent validation load also passed the static
criterion, but is not used for these timing comparisons.

Local validation additionally passed the exhaustive 256-marker JPEG test, the
summary helper self-test, region-filter regressions, all-targets checking and
strict Clippy. CI retains an explicit bounded timeout with headroom for the
exhaustive JPEG test and real software-rendered sessions.

## Publication boundary

Raw reports, raw logs, exact frame/marker traces, source hashes, failure messages,
and environment paths remain private. The summarizer has a bounded schema,
rejects duplicate JSON keys, invalid integer types and changed workload metadata,
and supports an explicit aggregate publication allowlist. CI uploads only that
allowlisted aggregate JSON, including failed-session status and missing counts.
The new helper's self-tests cover missing/late/cleanup samples, the exact 8-second
boundary, pre-commit rejection, uncertain markers, observed-only quantiles,
static regressions, malformed identities, and private-field exclusion.
