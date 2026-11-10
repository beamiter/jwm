# Read-only lossless scheduling: real-pipeline comparison design

This document defines the next comparison, not a successful result. The earlier
per-tile offline policy experiment established static-region fidelity under a
model clock; it did not measure real transport or presentation deadlines.
A subsequent real blinking-content regression exceeded its original 500 ms
presentation deadline. That deadline remains unchanged in the regression test.

## Arms and fixed workload

Run the same CI-owned Wayland source, authenticated transport and software-GL
viewer with the same limits, FPS, credits, dimensions and content schedule:

1. **Off:** neither endpoint negotiates static lossless repair.
2. **Legacy:** explicitly negotiated lossless repair, successful PNG flushes
   separated by 100 ms, without the proposed quiet/discard policy.
3. **Proposed:** explicitly negotiated repair with a 250 ms interval after a
   successful PNG flush, a 250 ms quiet period after any exact raw change, and
   pre-writer cancellation of an encoded PNG when a newer mailbox capture has
   different complete capture identity or RGB. Identical newer captures do not
   cancel it. Cancellation must consume neither record credit nor encoder commit.

The proposed arm is a **three-part policy bundle**, not a single-variable causal
experiment. It does not change the per-tile two-second stability requirement,
source permissions, frame limits or failure backoff. An already-written PNG
cannot be preempted; no hard real-time guarantee is claimed.

Use a fixed approximately eight-second, 500 ms update schedule. Do not delay
submission of the next generation until the previous one is presented. A two-state
black/white marker cannot establish generation identity if a frame is delayed by
two periods. Instead use a finite unique identifier inside the same 16×16 dynamic
tile, with independently verified JPEG-tolerant decoding and an explicit unknown
classification. Verify all identifiers through the production JPEG codec before
the real-pipeline run; do not infer identity merely from the expected generation.

Keep static content, dynamic tile size and fixture processing identical across
arms. Off is not expected to converge to zero static SSE. Preserve the original
strict regression separately; the measurement run retains late and missing
samples instead of aborting at the first deadline violation.

## Required bounded evidence

For each planned generation retain, against one process-monotonic origin:

- scheduled source-update instant;
- source preparation start and actual `wl_surface.commit` dispatch instant;
- identified marker generation and exact decoded/presented frame sequence;
- GL/readback start and end, validation completion and presentation-feedback send;
- the client's actual presentation-completion observation for that exact sequence.

Use actual sequence-associated presentation timestamps, not the later polling
instant that happened to notice them. Log unexpected/older identifiers as such;
never relabel them as the currently expected generation. A missing marker is a
missing sample, not zero latency. Keep late counts and the latency distribution's
sample denominator together so successful-only quantiles cannot hide drops.

Report both **scheduled update → presented** and **actual source commit →
presented**. Their difference exposes delayed source work inside the shared
software fixture. Keep preparation, GL, validation and feedback stages separate:
the fixture runs multiple compositors and software rendering in one environment,
so an aggregate delay is not automatically network or encoder delay.

Also record static-region SSE and time to exact convergence, per-codec counts,
tile/source payload bytes with an explicit byte boundary, actual JPEG/PNG encode
timings, and superseded-PNG discards. The new RGB comparison's cost must not be
silently excluded from a claimed CPU benefit. Preserve finite raw samples and
policy/configuration metadata, including build profile and source identity.

## Interpretation and acceptance

The useful outcome is improved or non-regressing content-update completion while
static text still converges at a bounded cost. Compare all three arms, not only
two successful subsets. Do not impose an artificial exactness requirement on Off,
or trade an unbounded convergence delay for a better update percentile.

A single ordered CI run is diagnostic, not statistically significant proof.
Repeated/alternated runs are needed before attributing a difference to the policy
rather than runner load. The real strict regression must pass without increasing
its deadline. Failure records remain part of the evidence, and the candidate
must not be published as an improvement merely because another run is green.

This is CI-owned software rendering, not physical GPU/DRM, real user input,
network-WAN performance or hardware input-to-visible acceptance. Test-only policy
selection must remain per-host-thread/session and cannot become a production
permission or hidden environment switch.

## Private report summarization

The fixture's `schema_version: 1` JSON uses the benchmark identifier
`static_lossless_scheduler_tcp_ab`. Summarize a private report without copying it
into the repository:

```sh
python3 scripts/summarize-lossless-scheduler-ab.py PRIVATE_REPORT.json > PRIVATE_SUMMARY.json
```

The summarizer bounds input to 16 MiB, six sessions, 16 generation markers and
4,096 frame observations per session. It rejects duplicate JSON keys and invalid
metric types. It excludes frames at or before `baseline_sequence`, readbacks
that started before that generation's `commit_issued_ns`, and presentations
before that commit. `commit_completed_ns` describes dispatch completion, not
an exact compositor-processing timestamp; the reported commit latency begins
at `commit_issued_ns`.

It retains failure descriptions, generations never submitted, submitted markers
never presented and presentations later than their original scheduled 500 ms
deadline. Quantiles use nearest-rank observed samples and retain their sample
count; they never replace missing samples with zero. `host_stats.sent_bytes`
means record payload including the source envelope, not Noise/TCP wire bytes.
Encoding time/counts include attempts discarded before sending. Encoded, decoded
and actually presented counts must not be conflated.


## Publication boundary

A public repository's CI artifacts are public, not private storage. The workflow
keeps raw JSON, raw test output and parser diagnostics only in `runner.temp` for
that run. It uploads **only** `--public-summary` output: an explicit allowlist of
aggregate counts, latency quantiles, convergence time and bounded host counters.
That output excludes failure strings, individual marker/frame traces, paths and
environment details. Raw evidence must not be copied into the repository or
uploaded as an artifact; any later retrieval requires an appropriately authorized
tool. A failed run remains visible through success/count fields without exposing
its raw failure trace.


### Aggregate stage attribution

The public allowlist also retains per-session nearest-rank count/p50/p95/max
summaries for the following stages. The two sessions of an arm remain separate;
the summarizer does not pool away run-to-run or ordering differences.

| Aggregate | Interval | Population |
| --- | --- | --- |
| `source_schedule_delay` | scheduled tick → source preparation start | Every submitted generation, including missing/late presentations |
| `source_preparation` | preparation start → commit issued | Same submitted generations |
| `commit_dispatch` | commit issued → dispatch completed | Same submitted generations |
| `fixture_stages.gl_readback` | GL start → GL complete | Every recorded post-baseline frame, including unknown markers |
| `fixture_stages.validation` | GL complete → validation complete | Same post-baseline frames |
| `fixture_stages.validation_to_feedback` | validation complete → feedback call start | Same post-baseline frames |
| `fixture_stages.feedback_dispatch` | feedback call start → dispatch return | Same post-baseline frames |
| `fixture_stages.feedback_started_to_presented` | feedback call start → actual sequence-associated presentation timestamp | Post-baseline frames with a recorded presentation |

Each frame's GL/validation/feedback durations are calculated from that frame's
own timestamps before aggregation. Source delay is signed rather than clamped.
A presentation can occur during feedback dispatch, so its timestamp may precede
`feedback_sent_ns`; the analysis does not incorrectly treat the dispatch return
as the beginning of that wait. Quantiles of different stages are **not additive**,
and counts expose different populations. An empty population has null quantiles,
not zero duration. None of these aggregates contains paths, environment, failure
strings or individual timestamps/traces.

`unknown_marker_frames` counts post-baseline frames whose marker could not be
identified, including frames without a presentation observation; they still
contribute to the applicable fixture stage durations. Unknown markers cannot
satisfy generation-latency matching. Exact-static convergence independently
requires static SSE=0, an actual presented timestamp and sequence greater than
the baseline. Its null value means not observed to converge, never zero ms.


### Fixed observation window versus cleanup

The experiment schedules updates for eight seconds and allows one second of
normal drain. The fixed window ends at **9,000,000,000 ns**, inclusive, relative
to the experiment origin. Shutdown may still collect presentation observations
for up to another six seconds. Therefore unprefixed presentation counts and
quantiles describe eventual observations, including cleanup, not solely the
nine-second window.

`within_window_presented_generations`, `within_window_missing_after_submission`
and the two `within_window_*_to_present` distributions use only presentations
at or before the nine-second boundary. `cleanup_only_presented_generations`
separately counts submitted generations first presented after that boundary.
A cleanup-only presentation remains missing in fixed-window coverage.
`within_window_first_static_exact_presented_ns` similarly excludes late cleanup
convergence; the unprefixed exact time retains the eventual observation.

The experiment origin is established after initial JPEG presentation/setup.
Static pixels may already have aged during setup, so experiment-relative exact
time does **not** measure two seconds from the first source capture. Whole-session
host counters include startup and shutdown and all encoded/discarded attempts;
dividing them by nine seconds would not yield a valid experiment throughput.
These limitations apply independently to every arm/session.


The allowlist explicitly reports on-time generation counts (eventual and
fixed-window), in addition to late/missing counts. `static_quality_eventual` and
`static_quality_within_window` report the number of actually presented frames
with nonzero static SSE after the first actually presented exact frame, plus the
maximum subsequent static SSE. Their populations are independently bounded by
the stated window. Both fields are null if no exact presentation was observed;
zero regression after convergence must not be confused with never converging.
This detects later quality regressions that a first-exact timestamp alone hides.
