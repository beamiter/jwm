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
