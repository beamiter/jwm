# Native input-queue wake experiment, 2026-11-10

Complete raw JSON reports are versioned here with their precise commit, workflow
and job provenance. The experiment is at `f9b104e5d378eabaef597c99ef2bd0cf44ba4b47`.
`same-process-abba.json` is the comparison; `candidate.json` is a separate ordinary
single-arm run, not its baseline. All inputs and visual markers are fixture-owned.

## Same-process comparison

A disables the new native input wake; B enables it. Four fresh sessions run in
A/B/B/A order, each with 2 warmups and 16 measured responses. Both arms retain the
same binary, process, FPS, capture-slot accounting, input authority, response
workload and controlled presentation chain. None uses the rejected retained-slot
scheduler. Each arm therefore has 32 measured samples and 4 warmups.

| Observed stage | A p50 (ms) | B p50 (ms) | B minus A (ms) |
| --- | ---: | ---: | ---: |
| Injection to target receipt observed | 10.90 | 5.56 | -5.34 |
| Application commit to GL verification | 33.58 | 36.82 | +3.24 |
| Injection to exact presentation observed | 47.52 | 45.28 | -2.25 |

Total p95 was 63.52 ms for A and 49.18 ms for B. The two order-block receipt
median differences were -11.74 and -5.33 ms; total median differences were -18.79
and -1.89 ms. Both order blocks improved, but their sizes vary substantially.
The commit-to-GL median regressed in this run; the improvement is not uniform
across stages. These are descriptive small-sample observations, not statistical significance,
a guaranteed speedup or a physical input-to-photon claim.

Baseline sessions recorded 88/89 activity-wait calls and zero wake operations.
Candidate sessions each recorded 107 waits, 152 drain calls, 20 notification
attempts and 19 drained signals. Their final teardown hint need not be consumed.
The additional notification/drain work is a real tradeoff; these counters neither
measure CPU utilization nor identify the cause of each poll return. They do
confirm that both comparison arms exercised their intended notification paths,
with no uncontrolled loop in this workload.

The three real-eventfd tests passed: coalescing/nonblocking drain, deterministic
publication after drain or queue scan, and per-session isolation/last-owner
resource lifetime. The same Wayland job also passed 885 remote tests, with 23
explicitly ignored tests and 46 CLI tests. This does not establish arbitrary
keyboard-flood fairness, physical GPU/DRM presentation or two-machine behavior.

The raw first-run report's generic quantile label mentions `n=16`, inherited from
the single-session helper. The explicit `n` fields correctly say 32 for pooled
arms, and nearest-rank values were calculated over all 32 observations. Later
report labels were corrected to avoid that wording ambiguity; raw evidence is
preserved unchanged.
