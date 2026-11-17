# Controlled input response evidence, 2026-11-10

These are complete test-emitted JSON reports, wrapped only with exact commit,
workflow and job provenance. They survive the CI artifact's three-day retention.
They contain fixture-generated markers, not user input, credentials or screenshots.

The baseline measurement implementation is commit
`ea22a45af907ba4f1132dc53d388f05490521fc7`. Its independent candidate and master
runs are recorded separately. They are **not** an A/B speed comparison.

The experimental retained-frame scheduling implementation and its reproducible
ABBA test are preserved at commit `e29a48028dc22176fd99c79d215b75ec4771656d`.
The production change was rejected and reverted after the same-process A/B result.
Check out that exact experiment commit to reproduce the `_abba` test described
in its benchmark document. Current production behavior remains the baseline.

## Decision from the same-process experiment

Each of four independent sessions ran 2 warmups and 16 measured responses in
A/B/B/A order. A is the old scheduler, B avoids charging a capture slot while a
native proof is retained, using a bounded 20 ms event-pump wait. Configuration,
process, binary, input workload, capture, JPEG and presentation paths otherwise
match. The fixture verifies a unique visual response caused by each real native
click before accepting its exact presented frame.

| Observed stage | A p50 (ms) | B p50 (ms) | B minus A (ms) |
| --- | ---: | ---: | ---: |
| Injection to target receipt observed | 10.95 | 17.41 | +6.46 |
| Application commit to GL verification | 38.22 | 38.20 | -0.01 |
| Injection to exact presentation observed | 51.34 | 58.11 | +6.76 |

The two order-block total median differences were +6.66 and +6.80 ms. Total p95
was 54.67 ms for A and 59.16 ms for B. Thus this implementation did not improve
the controlled workload and made it slower, despite its functional regressions
passing. It is **not shipped as a performance improvement**. These observations
point at the changed input-polling cadence, not a demonstrated capture/encoding
benefit. No post-hoc samples were removed to reverse that decision.

This is a small debug-build, software-renderer-requested, single-process loopback
experiment. It includes fixture dispatch, GL readback and synthetic presentation
feedback overhead. It does not measure physical input-to-photon, real two-machine
networking, GPU/DRM presentation or broad workload performance. No statistical
significance or generalizable latency claim follows from these samples.

`retained-candidate.json` is the experiment's ordinary single-arm run; only
`retained-abba.json` supports the same-run A/B comparison. Every report retains
warmups, measured samples, stage observations, environment requests and counts.
