# Static lossless scheduling: first real-pipeline comparison

Candidate `0987a0d153aed65baba3bcce9ab6ef262197375e` passed [Wayland](https://github.com/beamiter/jwm/actions/runs/42894832038), [X11](https://github.com/beamiter/jwm/actions/runs/42894832040), and [general CI](https://github.com/beamiter/jwm/actions/runs/42894832261). The unchanged strict 500 ms blink and popup/resize integration gates passed.

`first-aggregate.json` retains only allowlisted aggregate synthetic measurements, not raw traces or failure logs. Six symmetric same-input sessions each scheduled 16 generations at 500 ms intervals. All 96 were presented. Old scheduling had one late generation across its 32 samples; proposed scheduling had none. Per-session maximum scheduled-to-present latency was 533/468 ms old versus 420/428 ms proposed. Lossless-off was 294/287 ms.

This is a tradeoff, not an across-the-board speedup. First exact static presentation, measured from the experiment origin after initial JPEG presentation, moved from 4.34/4.65 seconds to 7.09/6.59 seconds. Both enabled policies sent 242,007 source-envelope payload bytes per session; off sent 220,191. These are not full transport bytes. Encoding counters include attempts; whole-session costs include startup and shutdown.

The renderer, both displays, source injection and validation share a synthetic process. Scheduled latency therefore includes fixture source scheduling, GL readback and validation, not just transport. This first summary does not expose those stage distributions or distinguish cleanup-time confirmations. A follow-up summary adds those aggregate distinctions; do not infer their values from the first report. Two sessions per policy are limited evidence, not a physical-GPU or two-machine latency guarantee. Continuous changes faster than 250 ms can defer otherwise-stable background repairs indefinitely. Lossless remains explicit opt-in and read-only.


## Second comparison and merged status

`second-aggregate.json` preserves only the authorized aggregate from candidate
`204c4a5a1ae38a26510160490318d383f33e8c62`, Wayland workflow
[42897397006](https://github.com/beamiter/jwm/actions/runs/42897397006).
The candidate passed all three required CI workflows and is merged on master;
the completed mainline rerun is recorded below.

All 96 scheduled generations were presented on time within the nine-second
window. Unknown markers, cleanup-only confirmations and post-exact static SSE
regressions were all zero. Off did not reach static exactness, as expected.
Each pair below lists the separate sessions, not pooled samples:

| Policy | Maximum scheduled→presented ms | First static exact seconds |
| --- | --- | --- |
| Off | 189.00 / 185.64 | Not observed / Not observed |
| Legacy | 239.14 / 277.67 | 3.112 / 3.535 |
| New policy (`proposed` label) | 249.97 / 199.88 | 5.595 / 5.456 |

Both enabled policies again sent 242,007 source-envelope payload bytes per
session and decoded eight PNG records; off sent 220,191 bytes. No stale PNG was
dropped in this workload, so it does not establish the performance contribution
of the supersession guard. New-policy comparison cost totaled approximately
0.021/0.018 ms per session. Fixture validation alone had per-session median
costs around 45–46 ms, reinforcing that aggregate latency is not purely the
production network path. Host costs include setup/shutdown, not nine-second
throughput; exact time starts after initial JPEG setup, not first capture.

Compared with the first run, the second does not show a consistent per-session
latency improvement: both legacy and new scheduling met every deadline. It does
reproduce slower static convergence for the interaction-prioritizing policy.
Two runs with two sessions per arm are limited synthetic evidence, not a general
speedup or hardware guarantee. No raw traces, failure strings or environment
records are included in either aggregate.


## Third run: mainline regression counterexample

`mainline-aggregate.json` preserves the allowlisted mainline result for
`204c4a5a`, [Wayland run 42899339985](https://github.com/beamiter/jwm/actions/runs/42899339985).
All four mainline CI workflows passed. Passing the strict gate does not establish
a scheduling improvement: the third synthetic run is a concrete counterexample.

| Policy | Maximum scheduled→presented ms, separate sessions | Maximum commit→presented ms | First static exact seconds |
| --- | --- | --- | --- |
| Off | 214.33 / 212.35 | 206.39 / 208.85 | Not observed / Not observed |
| Legacy | 246.43 / 243.43 | 244.69 / 242.48 | 3.587 / 3.829 |
| New policy | 335.18 / 322.22 | 333.22 / 320.53 | 5.584 / 5.602 |

Both new-policy sessions had worse update maxima than both legacy sessions.
The same direction appears after measuring from actual commit, so delayed source
submission alone cannot explain this run. All generations still met the 500 ms
gate. The enabled policies again each encoded eight PNGs and sent 242,007 payload
bytes per session; no stale PNG was discarded. Total encode costs were similar
(legacy 469/476 ms, new 465/477 ms), so these aggregates do not establish an
encoding-work reduction either.

Across the three runs, the first favored the new policy under a late-update
case, the second was mixed, and the third regressed in both new-policy sessions.
The slower static convergence is consistent across runs. This evidence does not
justify a general “improved latency” claim. Lossless remains explicitly opt-in,
default off and limited to the authorized modes; disabling it remains available.
The next measurement should isolate quiet time from batch pacing before another
production scheduling decision. These remain synthetic software-pipeline
results, not hardware, WAN or two-machine acceptance.


## Factorial comparison: quiet time versus batch pacing

`factorial-aggregate.json` records candidate `1419c201`,
[Wayland run 42906349912](https://github.com/beamiter/jwm/actions/runs/42906349912),
job `130145216178`. All three candidate CI workflows passed. This is an
allowlisted aggregate only; no raw traces, failure strings or environment records
are included. The experiment does not change the production default.

Eight mirrored sessions scheduled 128 unique generations. All were actually
presented within the fixed nine-second window, with no missing or unknown
markers and no cleanup-only confirmations. 127 of 128 met the original 500 ms
update deadline. The second 0/100 session missed once; the measurement records
that miss rather than aborting. The separate strict regression also passed.
All four policies keep the supersession guard enabled, so 0/100 is not the
historical unguarded legacy implementation.

Each pair preserves the two individual sessions in execution order:

| Quiet / pacing ms | Scheduled→presented p50 ms | Scheduled→presented maximum ms | First static exact seconds |
| --- | --- | --- | --- |
| 0 / 100 | 300.22 / 258.05 | 401.92 / **521.82** | 5.088 / 4.593 |
| 0 / 250 | 285.28 / 293.94 | 465.75 / 412.87 | 5.149 / 5.144 |
| 250 / 100 | 295.14 / 286.36 | 446.03 / 420.92 | 7.102 / 8.101 |
| 250 / 250 | 300.67 / 302.71 | 426.50 / 442.25 | 7.225 / 7.100 |

Every session sent 242,032 source-envelope payload bytes, not total transport
bytes. The first 0/100 session encoded nine PNG attempts and discarded one;
the second 250/100 session encoded ten and discarded two. The other sessions
encoded eight. Encode costs include these discarded attempts and whole-session
setup/shutdown, rather than only the nine-second experiment. Every session had
zero observed nonzero-static-SSE presentations after its first actual exact
presentation. Exact times are relative to the post-initial-JPEG experiment
origin, not the first capture.

In this run, 250 ms quiet time delayed static convergence without a consistent
response benefit. Pacing effects were mixed: 0/100 had both the lowest median
and the only deadline miss, in its second session. 0/250 is worth repeating as
a possible response/convergence compromise, not an established winner or a
reason to change defaults from this run alone. Compare quiet settings at fixed
pacing and pacing at fixed quiet time, retaining each session and the prior
three-run counterexamples. These are synthetic software-pipeline observations,
including fixture scheduling/readback/validation, not hardware or WAN claims.
