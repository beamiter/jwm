# Per-tile stability: offline policy comparison and prior CI history

`offline-policy-ab.json` is the complete raw output from the production tile
encoder/decoder test `offline_blinking_tile_exact_stability_ab`. Both arms use
**the same candidate codec binary**: one emulates the former global two-second
eligibility rule; the other uses per-tile stability. This is not a historical
binary A/B and is not a transport, GL, physical display or application benchmark.

Both receive exactly the same 101 synthetic 160×96 RGB frames, at model times
0..10,000 ms in 100 ms increments. One 16×16 tile alternates every 500 ms;
other pixels are fixed synthetic colored strokes. The observation clock is
simulated, without sleeps. The shared input-sequence SHA256 is
`da1f3b7eddd55eda2f64b57963521a1e9c62e2cdd18ed279783561fbf99a02b1`.

| Metric | Global eligibility model | Per-tile eligibility |
| --- | ---: | ---: |
| First static-region SSE=0, model ms | Not reached in 10,000 ms | 2,100 |
| Final static-region SSE | 6,041,782 | 0 |
| Total tile payload bytes | 23,760 | 25,137 |
| PNG records | 0 | 1 |
| Observed cursor generations | All 0..20, ordered | All 0..20, ordered |
| Emitted-record planning samples, summed ms | 1.858354 | 1.624483 |
| Encode samples, summed ms | 15.065497 | 18.074123 |
| Decode samples, summed ms | 17.478895 | 17.793019 |

Thus this run paid 1,377 additional tile-payload bytes to converge the static
region while retaining each blink. Byte counts exclude source envelopes,
authenticated framing and TCP. Cursor state is threshold-classified, not claimed
pixel-exact JPEG output. Timing samples are real debug wall durations around CPU
operations, **not** the simulated convergence clock. Planning totals include
emitted records only; suppressed rounds have no report row. One ordered run
cannot establish a throughput/latency improvement or statistical significance.

## Prior integration validation and failure retained separately

`previous-global-cadence-ci.json` preserves the preceding whole-frame-stability
candidate's real CI evidence (commit `a838cf30`, tree `40fb6eb8`): owned Wayland,
authenticated TCP and software GL. Its first JPEG had SSE 353,693,133; eight
bounded batches reached full 800×600 equality at approximately 4.088 seconds;
a later 64×64 +1-red change reconverged in approximately 2.312 seconds. These are
fixture convergence observations, not input-to-visible latency or GPU acceptance.
They do not prove the new per-tile TCP/GL path.

`previous-first-ci-failure.json` retains the earlier failed integration run.
The initial two-second delay between every successful batch exceeded the
18-second fixture deadline. The subsequent candidate used a 100 ms successful
flush interval while retaining a two-second failure backoff. This history is
preserved rather than interpreting the failed run as successful convergence.

`metadata.json` records the offline candidate's dirty tree and source hashes.
New per-tile actual TCP/GL validation must be reported independently.
