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
