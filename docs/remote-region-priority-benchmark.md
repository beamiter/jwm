# Test-only latest-capture priority experiment

`RegionPriority` is a private, `cfg(test)`-only scheduling policy. It is not
selected by production code or exposed as a production option. This experiment
extends [the region-deadline benchmark](remote-region-deadline-benchmark.md);
it does not change permissions, protocol, display-credit rules, JPEG quality,
per-tile stability, PNG batch bounds, or the existing 500 ms workload gates.

## Policy under test

After waiting for normal display credit, a pending region follow-up gives
priority to a newer capture already queued in the latest-frame mailbox. This
avoids sending the retained follow-up ahead of fresh dynamic content. With no
new capture queued, the retained follow-up can still use the available credit.
The mailbox decision must remain atomic with respect to capture replacement and
closure, and must preserve the existing bounded ownership and stale-work rules.
This is a scheduling experiment, not permission to send without credit or to
hold an unbounded capture queue. Static convergence and dynamic responsiveness
must both be measured; prioritizing fresh captures could delay static repair.

## Fixed workload

The workload and metadata are unchanged: 800×600 source, 12 FPS host, quality-60
JPEG, 80 generations at scheduled 100 ms intervals, 8 seconds active observation,
1 second drain, exactly 32×16 dynamic pixels and 479,488 static pixels, two-second
per-tile stability, and at most 65,536 pixels per PNG batch. Source hashes must
match across arms. Actual Wayland viewer GLES readback and controlled presentation
feedback run over authenticated loopback TCP with software EGL/llvmpipe. These
measurements are not physical-display latency or a WAN performance guarantee.

The benchmark identity is
`static_lossless_region_priority_continuous_tcp_ab`. Its six sessions have the
fixed mirrored order `new250`, `region_deadline`, `region_priority`,
`region_priority`, `region_deadline`, `new250`. The original deadline benchmark
continues to accept only its original four-session arm/order schema. Partial and
failed reports remain readable and retain their missing-generation denominator;
they cannot establish dynamic non-regression.

```sh
export EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 JWM_REQUIRE_HEADLESS_GL=1
export XDG_RUNTIME_DIR="$(mktemp -d)"
export JWM_REMOTE_REGION_PRIORITY_REPORT=/path/to/private/region-priority.json
base=remote::host::wayland_viewer_integration::host_lossless_integration::scheduler_benchmark
scripts/test.sh --no-default-features \
  --features remote-wayland,backend-wayland-udev --lib \
  "$base::actual_static_lossless_region_priority_continuous_measurement" \
  -- --ignored --exact --test-threads=1
python3 scripts/summarize-remote-region-deadline.py --self-test
python3 scripts/summarize-remote-region-deadline.py \
  "$JWM_REMOTE_REGION_PRIORITY_REPORT" > /path/to/private/summary.json
python3 scripts/summarize-remote-region-deadline.py --public-summary \
  "$JWM_REMOTE_REGION_PRIORITY_REPORT" > /path/to/aggregate-summary.json
```

Run timing experiments sequentially without competing validation jobs. Retain the
existing exhaustive Gray-marker JPEG and boundary tests as correctness gates.
No performance outcome is asserted here merely because the experiment exists.

## Conservative acceptance assessment

Both New250 controls must reproduce static starvation in the active window. Both
RegionDeadline controls and both RegionPriority candidates must reach exact
static pixels within that window through multiple presented PNG batches and
present later animation committed after exactness. No observed exact-to-inexact
regression is allowed, including recorded drain/shutdown observations.

The summary adds `dynamic_non_regression`. It first requires all six successful
sessions and all their static gates. Then each of the two RegionPriority sessions
is compared separately against the best value of each metric across all four
controls, including both New250 and both RegionDeadline sessions:

- Active-window scheduled-to-present p95 and maximum.
- Active-window commit-issued-to-present p95 and maximum.
- Active-window missing planned generations, out of all 80.
- Late presented generations (>100 ms), including cleanup-only presentations.

Every metric in both candidates must be no worse. Ties pass. Per-metric minima
can come from different control sessions, making this stricter than a pooled
average or comparison with a convenient slow control. The original four-session
benchmark uses the same assessment with RegionDeadline as the candidate and
both New250 sessions as controls.

An incomplete/failed session, failed static gate, or absent latency observation
produces `no_claim`. Present but missing generations are never imputed as zero
latency: their counts are compared explicitly while quantiles remain observed-
sample-only. A worse candidate metric yields `non_regression_not_established`.
Only all passing comparisons yield `observed_non_regression`, a descriptive
result for this run, not statistical proof, a hard real-time guarantee, or
approval to enable the policy in production. Source preparation, source commit
gaps, fixture-stage costs, payload and encode counters still require inspection.

## Privacy and verification

The existing 16 MiB input bound, strict integers, duplicate-key rejection,
non-finite JSON rejection, fixed metadata, source hashes, and session ordering
checks remain in force. Public output uses explicit nested allowlists, including
assessment metrics. Raw traces, source hashes, failure messages, environment
paths, and future diagnostic fields stay private. Publish only the output of
`--public-summary`, never raw reports or logs.

The helper self-test covers both benchmark schemas, invalid arms/orders,
duplicate and excess sessions, benchmark mismatch, both candidates and every
comparison metric, best-control selection, missing data, static failure, and
nested private-field exclusion in addition to the original timing/quality tests.

## Initial local results (2026-11-12)

Two sequential clean six-session software-GL runs passed all static criteria.
All four RegionPriority sessions converged in 4.217, 4.044, 3.975, and 3.871
seconds, retained exact static pixels, and presented later animation. Both
runs' New250 controls remained inexact. These are synthetic loopback/llvmpipe
observations, not physical-display or WAN latency measurements.

Scheduled-to-present p95/max milliseconds and missing active-window generations
(out of 80), in the fixed six-session order:

| Run | New250 | RegionDeadline | RegionPriority | RegionPriority | RegionDeadline | New250 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 p95/max | 269.92/303.11 | 277.70/454.60 | 348.41/487.79 | 331.79/520.95 | 305.73/610.21 | 265.59/335.83 |
| 1 missing | 26 | 25 | 31 | 34 | 32 | 30 |
| 2 p95/max | 378.86/522.49 | 285.69/331.89 | 259.18/294.71 | 228.75/240.86 | 271.13/282.10 | 283.79/291.91 |
| 2 missing | 40 | 34 | 24 | 23 | 25 | 33 |

Both runs returned `non_regression_not_established`. The first run's candidates
were worse than production on p95/max and missing generations; the second run
improved p95 and missing counts but did not satisfy every conservative metric.
This mixed evidence does not justify enabling either experimental policy.
A preceding run overlapped focused regression execution and is excluded from
these timing comparisons. Its static gates passed, but it is not a clean
performance comparison. Raw reports and logs remain private.

## Optimized repeated evidence

Debug results above are useful correctness checks, but they do not establish
release-build latency. The existing optimized Wayland CI job now reuses its
`profiling` executable for **two sequential complete six-session runs**, with
identical fixed inputs, order, timing and safety gates. This changes neither the
release profile nor the production scheduler. Both results are retained as
separate allowlisted artifacts when their reports can be parsed, including
reported session failures; missing/invalid reports fail the step and never
produce a successful summary. Do not select the
faster repetition or pool away a slow candidate. A test failure in either run
fails the step. A dynamic `non_regression_not_established` result is still valid
evidence, not a test failure and not permission to enable the policy.

New raw reports and public summaries include the compile-time `debug_assertions`
boolean. Historical reports with no field remain `null` in summaries. The
optimized CI command uses the fixed `--profile profiling` flag and the summary's
`--require-no-debug-assertions` guard rejects missing, malformed or true evidence.
The boolean alone does not attest a compiler optimization level; the exact Cargo
command and unchanged profile are part of the evidence. Debug CI remains in
place as an independent correctness gate.

For local repetition use the command above with `--profile profiling`, distinct
private report paths for each run, and the summary flag
`--require-no-debug-assertions`. Run sequentially without concurrent compilation
or benchmarks. Inspect both repetitions' source preparation/commit delays,
fixture stage times and host encode counters before attributing latency to
scheduling. Software GL, controlled presentation and loopback are still not a
physical GPU/display, WAN or real two-computer benchmark. Two repetitions do not
establish statistical significance or authorize production promotion.
