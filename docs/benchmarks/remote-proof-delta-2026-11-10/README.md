# Native proof delta experiment, 2026-11-10

First candidate: `dccc76cf96f3b268c0c9c502efa7df8fabf3e962`.
[Wayland CI run](https://github.com/beamiter/jwm/actions/runs/42826318399),
job `129864450355`. [Exact parsed report](first-candidate.json).

Same-process ABBA, 4 sessions, 2 warmup + 16 measured inputs per session,
32 measured per arm. Each sample index uses identical source RGB hashes.
A resets the encoder to a keyframe for each native proof; B preserves its
reference and forces an exact-damage delta/zero-dirty record. Both keep real
proof, frame-sequence and presentation gates.

| Measured p50 | A forced keyframe | B delta | B minus A |
| --- | ---: | ---: | ---: |
| Encoded payload bytes | 3094 | 1028 | -2066 |
| JPEG stage ms | 7.661911 | 0.319799 | -7.342112 |
| Application commit to verified GL ms | 40.833257 | 53.690767 | +12.857510 |
| Input injection to observed presentation ms | 48.279407 | 61.723221 | +13.443814 |

Both total-latency block median differences were regressions (+13.363791 and
+11.552347 ms). This is not an end-to-end latency improvement. Functional gates
passed, including a held-presentation zero-dirty proof test, but this candidate
was not promoted to master on these results. Follow-up stage timing is needed
to locate the regression before changing production scheduling.

B static controls emitted 57-byte payloads with zero dirty tiles and no JPEG
work, yet required a new native proof and actual presented sequence. Full-change
controls carried every tile in both arms. Payload accounting includes the
source envelope, not encrypted record framing/padding or TCP. JPEG accounting
covers successful published proof records only, not discarded encoding work.

These are small descriptive samples on owned loopback TCP/Wayland servers and
software/offscreen GL. No physical GPU, DRM, two-machine or input-photon claim.
The retained report includes all warmups, measured samples and controls; no
negative samples were removed.

## Timing-only diagnostic rerun

Candidate `e781b0cdd5a45bfb0b8bf5dd91bf7d48456995cb`,
[Wayland run](https://github.com/beamiter/jwm/actions/runs/42829326628),
job `129874904600`. [Complete report](timing-candidate.json). Production
scheduling was unchanged; this added test-only, exact-scope Instant observations.

Measured p50 capture-start wait after app commit was A 2.780836 ms versus
B 30.888741 ms; ACK-to-Ready publication was A 16.660836 versus B 2.299164 ms.
These per-stage quantiles are descriptive and are not additive. The complete
per-sample pipeline offsets, including signed overlapping capture time, remain
in the report. Scope joins use native serial, source epoch, geometry epoch and
wire sequence; event-vector insertion order is not a clock order.

Input-to-visible-presentation p50 was 47.283679 versus 61.844578 ms, but
input-to-actual-client-Ready p50 was 67.476033 versus 68.321643 ms. The latter
p95 was 68.337936 versus 74.904937 ms. Both block median Ready-cycle differences
were positive (+0.734190 and +0.973934 ms). The complete-cycle mean decreased
slightly (67.633828 to 66.546771 ms), which does not override worse medians/tails.
This is not evidence of faster interaction. Faster encoding changed where the
closed-loop workload waited relative to the capture rate limit; a possible
ACK notification fix needs its own controlled comparison.

Wayland diagnostic checks passed. General CI exposed an existing wrong-PSK
TCP fixture shutdown race (`NotConnected` after the peer had already closed),
so this diagnostic commit did not satisfy all CI gates and was not promoted.
Its fix must preserve the authentication rejection assertions.

## ACK wake alone is insufficient

Candidate `32eadbbd17e60d53bae86ac2b71ecb7de44f6b87`,
[Wayland run](https://github.com/beamiter/jwm/actions/runs/42833425813),
job `129889183865`. [Complete symmetric eight-session report](ack-wake-candidate.json).
All 144 inputs and 16 static/full-change controls succeeded. X11 and Wayland
gates passed. General CI passed on attempt 2 after retrying only an unrelated
player single-instance readiness timeout; the first failure is not erased.
The four configurations
had these measured p50 values (32 per configuration):

| Policy | Input to visible ms | Input to Ready ms | Payload bytes | JPEG ms |
| --- | ---: | ---: | ---: | ---: |
| Old ACK + keyframe | 56.932026 | 67.923885 | 3412 | 8.050972 |
| Old ACK + delta | 62.087115 | 67.921864 | 1107 | 0.391709 |
| ACK wake + keyframe | 64.591691 | 67.862140 | 3412 | 8.294332 |
| ACK wake + delta | 63.839790 | 67.142257 | 1107 | 0.375389 |

The new hint reduced presentation-to-Ready observation to about 3.27 ms, but
complete cycles remained around 67–68 ms and visible latency still regressed
against the original configuration. No default promotion was justified by this
result. The next experiment isolates the explicit retained-proof no-readback
outcome from real capture attempts while keeping the true capture FPS limit;
it is not the earlier blanket `NoChange` accounting experiment.

## Explicit retained-proof scheduling candidate

Candidate `aed00a7ba0ac427efdfceb9cd6f18ebb524d956f`,
[Wayland run](https://github.com/beamiter/jwm/actions/runs/42837135493),
job `129902237921`. [Complete report](final-candidate.json).
Order is Alegacy/Blegacy/Anew/Bnew/Bnew/Anew/Blegacy/Alegacy; A is forced
keyframe and B is exact-damage proof delta. Legacy disables ACK hints and
charges retained-proof polls as capture attempts. New enables ACK hints and
preserves the existing deadline only for explicit `RetainedProofBlocked`.
Both retain input eventfd wake, FPS limits, authorization and exact presentation
gates. This tests the combined pipeline, not each component in isolation.

All 144 samples and 16 static/full-change controls succeeded with identical
same-index source RGB hashes across all eight sessions. Comparing the original
configuration (legacy + keyframe) to the new default (new + delta), 32 measured
responses each:

| Endpoint | Original p50 | New p50 | Original p95 | New p95 |
| --- | ---: | ---: | ---: | ---: |
| Input to verified presentation ms | 56.900361 | 38.634912 | 59.054010 | 40.459254 |
| Input to actual client Ready ms | 67.743321 | 41.952107 | 69.471702 | 44.823911 |
| Encoded payload bytes | 3142 | 1087 | 3336 | 1739 |
| JPEG stage ms | 8.046269 | 0.343933 | 8.291493 | 1.104687 |

The new pipeline with forced keyframes also improved Ready p50 to 58.844111 ms;
its visible p50 was 55.568835 ms. Thus the result is not justified only by JPEG
savings or by moving Ready wait outside the measured cycle. Prior negative
experiments above remain available. The sample count, fixed symmetric order,
small 200x150 workload and software renderer still limit generalization.

Candidate gates: X11 run `42837135504` passed; Wayland run `42837135493`
passed with 901 remote tests plus 47 CLI tests, including the actual 250 ms
retained-proof hold/presentation regression and native ACK eventfd checks.
General CI run `42837135662` passed on attempt 2 after retrying only an unrelated
BlueZ confirmation-window fixture failure. No runtime authentication or safety
assertions were relaxed. Physical GPU/DRM and real two-machine acceptance remain
unverified. Mainline verification is recorded separately after promotion.
