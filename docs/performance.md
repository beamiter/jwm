# Performance contract (Phase 5)

This document is the contract behind `jwm-tool perf`: which scenarios are
measured, which regression budgets apply, and the labeling rules that make two
results comparable at all. The machine-readable side lives in
`tools/perf_contract.rs` (schema, budgets, comparison) and is what CI or a
reviewer actually runs; this page explains it.

Two principles carry the whole contract:

1. **Every result is labeled.** A baseline permanently records the CPU, GPU,
   driver, kernel, backend, renderer API, resolution, and a fingerprint of the
   effective configuration file. `jwm-tool perf compare` refuses — it does not
   merely warn — when either side is missing label fields or when the labels
   identify different systems or configurations. Unlabeled results from
   different machines are different benchmarks, never comparable points on one
   curve.
2. **Skips are recorded, not silent.** A scenario the running session cannot
   measure (no compositor, counter not compiled in, no input timestamps) is
   written into the baseline as `skipped` with its reason. When the baseline
   has no measurement either, comparison reports the pair as *not
   comparable* instead of quietly passing it. A skip only in the candidate —
   the baseline measured the scenario — is a regression, not a
   not-comparable pair, with two exceptions whose presence depends on how
   the session was recorded: `input_latency` (the session saw input before
   or during the window) and `allocation_steady` (a jwm built with
   `alloc-counter`). A candidate that lacks only those is reported as not
   comparable.

## Recording

```
jwm-tool perf record                # writes perf/baselines/<label>.json
jwm-tool perf record --out my.json --frames 300 --warmup 60 --idle-seconds 10
jwm-tool perf compare baseline.json candidate.json   # exit 1 on regression
jwm-tool perf budgets                                # print the budget table
```

Recording talks to the live session over the IPC socket and samples
`/proc/<pid>` for the idle scenario. Record on a quiet desktop: close video
players and animations, and do not interact with the session during the
sampling window. The `steady_frame` scenario measures the compositor under
*ambient* damage by default, which makes its absolute numbers workload
dependent — compare like against like (see the workload note below).

A session without an active compositor, or one that refuses to start the
benchmark (another benchmark already running, for example), does not abort
the recording: the compositor scenarios are written as `skipped` with that
reason and the baseline, idle measurement included, is still saved.

A benchmark that overruns its 300-second deadline is stopped rather than left
running. Its partial frame numbers are discarded (`steady_frame` is written as
`skipped`: the benchmark did not complete), but the report `benchmark stop`
returns still supplies the label's GPU, driver and resolution. An overdue
candidate therefore keeps the label of a completed baseline from the same
system, and `compare` prints its lost frame times as `[FAIL]`. A run that got
no report at all (no compositor, a refused start) labels the GPU and driver
from host fallbacks and the resolution from the extent the `get_monitors`
entries span. The fallbacks are the GPU model and driver version the NVIDIA
kernel module reports; elsewhere only the driver is known, as the name of
the DRM driver bound to `card0`. Those host strings are not the
`GL_RENDERER`/`GL_VERSION` strings a benchmark report carries, so `compare`
refuses such a run against a compositor baseline, as unlabeled or as a
different system, rather than printing `[FAIL]`.

On a composited session, `--waterlily-workload` first asks
`get_waterlily_status`. When WaterLily is unavailable (no Wayland backend has
it), no WaterLily worker is connected, or `toggle_waterlily` does not take
effect, the compositor scenarios are written as `skipped` with
`waterlily workload unavailable: <reason>` instead of measuring ambient
damage for a paced-workload baseline. An animation that is already running is
used as is and left running; WaterLily is switched back off only when the
recording switched it on, whether or not the benchmark then started.

## Scenarios

| scenario | roadmap bullet | metrics | source |
| --- | --- | --- | --- |
| `idle` | idle CPU and wakeups | `cpu_percent_avg`, `wakeups_per_s`, `rss_mb` | `/proc/<pid>/stat` + `status` deltas over the idle window |
| `steady_frame` | frame-time median/p95/p99 | `frame_time_{avg,p50,p95,p99,stddev}_ms`, `fps_avg`, `frame_samples` | compositor benchmark harness (`benchmark` IPC command) |
| `damage_redraw` | damage-area and redraw ratios | `dirty_fraction_avg_percent`, `dirty_regions_avg`, `dirty_region_merges_avg` | `get_metrics` sampled once per second across the benchmark window |
| `input_latency` | input-to-present latency | `input_latency_{p50,p95,p99}_ms` | benchmark harness when it observed input, else the compositor's rolling window |
| `allocation_steady` | allocation counts in steady-state frame production | `allocs_per_frame`, `frames_observed` | `allocations` counter deltas (requires a jwm built with `--features alloc-counter`) |
| `multi_monitor` | multi-monitor refresh-rate and mixed-scale behavior | `monitor_count`, `refresh_hz` | `get_monitors` + `get_metrics` |
| `direct_scanout` | direct-scanout entry/exit stability | `scanout_toggles_per_minute`, `scanout_active_end` | `direct_scanout_count` deltas across the window |

### Workload sensitivity

`steady_frame`, `damage_redraw`, and `allocation_steady` measure whatever
damage the desktop produced during the window. Two recordings taken minutes
apart can differ wildly (an idle desktop renders frames in bursts; a 60 Hz
animation paces them at the refresh rate) while both being *correct*. The
label cannot capture this, so the recording protocol must: record baselines
and candidates under the same conditions, and sanity-check `fps_avg` against
`refresh_hz` before trusting a frame-time comparison. For a deterministic
paced workload, `--waterlily-workload` enables the built-in continuous
animation for the duration of the benchmark window, or uses the one already
running (visible on screen either way; see Recording for when it is
unavailable).

## Budgets (v1)

`jwm-tool perf budgets` prints the authoritative table; in summary:

- **Ratios** bound drift against the baseline: frame-time averages and medians
  may grow at most 10%, p95 15%, p99 25%; input latency the same; the frame
  rate must keep at least 90% of the baseline; idle CPU and wakeups may grow
  at most 50%; RSS 30%; damage fraction 25%; allocations per frame 15%.
- **Absolute rails** apply to the candidate regardless of the baseline: idle
  CPU ≤ 10%, wakeups ≤ 600/s, RSS ≤ 2 GiB, damage fraction ≤ 100%, and
  direct-scanout flapping ≤ 120 toggles/minute. A zero baseline (for example
  direct-scanout on an X11 session that never toggles) is bounded only by its
  rail.
- **Exact** facts must not change between runs: monitor count and refresh
  rate. If they changed, the display topology changed and the run belongs to
  a different label anyway.

Violating any budget makes `perf compare` exit non-zero, so it can gate a CI
job or a release checklist. A candidate that lost a measurement the baseline
recorded — the scenario skipped or absent, or the metric missing — is a
violation too: it is printed as `[FAIL]` with the candidate's skip reason and
fails the gate, because a regression that stops the benchmark from running
must not read as green. The exceptions are `input_latency`, which exists only
when the session saw input, and `allocation_steady`, which exists only in a
jwm built with `alloc-counter`: a candidate that lacks only those is printed
as `n/a`, and a stalled benchmark still fails the gate through
`steady_frame`. Every other scenario fails closed. Otherwise `NotComparable`
(printed as `n/a`, never failing) is reserved for pairs where the baseline
itself has no measurement, for example `allocation_steady` skipped on both
sides, or a scenario the candidate gained.

## Baselines

Committed baselines live in `perf/baselines/`, one file per system label.
They are reference points for *that machine*: refresh them deliberately (a
new driver, kernel, or config is a new fingerprint and a new file), and never
edit the JSON by hand. The `allocation_steady` scenario stays skipped unless
the session runs a jwm built with `--features alloc-counter`; that build is
compile-checked in CI but intentionally excluded from default builds, since
counting costs one atomic increment per heap allocation.

The commented-out `[profile.release]` tuning block in `Cargo.toml` remains
disabled; per the roadmap it may only be enabled together with benchmark
evidence recorded through this contract.
