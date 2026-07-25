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
   written into the baseline as `skipped` with its reason. Comparison reports
   such pairs as *not comparable* instead of quietly passing them.

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
animation for the duration of the benchmark window (visible on screen while
it runs).

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
job or a release checklist. `NotComparable` entries (skipped scenarios,
missing metrics) never pass silently as green — they are printed, and the
overall verdict considers only genuinely evaluated budgets.

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
