# Offline tile codec replay

`jwm-remote replay-benchmark` emits one JSON object for five deterministic synthetic
scenes, using the production `TileEncoder` and `TileDecoder`. It does not open a
display, socket, key file or clipboard. This is a reproducible **codec baseline**,
not a desktop workload capture or an end-to-end latency benchmark.

```sh
cargo run --release --locked --no-default-features --features remote-x11 \
  --bin jwm-remote -- replay-benchmark --width 320 --height 180 \
  --frames 30 --warmup 3 --seed 1 --quality 75 > replay.json
```

Review the configuration before larger runs. Bounds are width 1..1920, height
1..1080, 1..300 measured frames per scene, 0..30 warmup frames per scene, JPEG
quality 1..100, and 300 million total generated pixels across the five scenes
(including warmup, conservatively using maximum dimensions). These bound workload,
not wall-clock runtime or total process RSS. Frames are generated one at a time;
there is no stored frame corpus. Defaults are intentionally small.

## Workload version 1

The seed controls an explicitly specified integer pixel pattern, without RNG
library or OS randomness. RGB cells are 8x8 pixels. Scenes run in fixed order:

- Idle: identical pixels after the initial keyframe; ordinary deltas suppress output.
- Sparse change: top-left 16x16 patch alternates black and white every frame.
- Scroll: pattern coordinates move vertically by seven pixels per frame.
- Full change: every frame uses a new deterministic pattern phase.
- Resize: alternate base dimensions and `(max(1,width-17), max(1,height-9))`
  every three frames, advancing the geometry epoch at each transition.

Each scene first runs its explicit warmup count with separate codec state. Measured
replay starts with fresh codec state, so its first keyframe is included. Warmup
primes code/caches, not retained measured codec allocations. No sleeps, network
pacing, ACKs, quality adaptation or artificial keepalives are involved. Suppressed
frames have plan samples but no encode/decode samples. Sequence numbers advance
only for emitted frames.

## JSON and timing semantics

Schema/workload versions, config, debug-assertion state and optional compile-time
`JWM_BUILD_COMMIT` (otherwise `unknown`) identify the run. Set that variable to a
reviewed source commit when building; it is a caller-supplied label, not verified
provenance and does not detect dirty sources. No environment dump or hostnames
are collected.

Each scene reports measured frame/emission/suppression counts, encoded payload
bytes (tile header/bitmap/JPEG, excluding transport framing), dirty tiles and
keyframes. Stage durations are microseconds with sample count, arithmetic mean,
and nearest-rank p50/p95/p99. Empty sample sets use JSON null, never fabricated
zero latency. Stages:

- `plan`: dirty detection and tile planning.
- `encode`: complete `encode_into` call, including payload assembly.
- `atlas_pack`, `jpeg`: internal encoder sub-stages, overlapping `encode`; do not add them to it.
- `commit`: reference-state advancement.
- `decode`: complete `decode_into`, producing the immutable tile snapshot.

Generation, correctness checks, lazy full-RGB materialization and frame destruction
are outside these intervals. Every emitted frame is checked for sequence, source
and decoded dimensions and valid RGB buffer length. JPEG is lossy; these are
structural checks, not pixel-equality or visual-quality claims. Report overhead,
allocations, cache state, CPU contention and build mode affect results. Use the
same build mode, source, configuration and quiet hardware for comparisons; retain
JSON from both runs. This tool alone establishes no speedup.

Cursor-plane motion, screen capture, scaling, input, network, encryption, queue
latency, X11/GPU presentation, clipboard and input-to-visible timing are outside
scope. Offline passing tests do not establish live desktop behavior or complete
the remote-control optimization roadmap.

Focused smoke tests (small dimensions, no live X11):

```sh
cargo test --locked --no-default-features --features remote-x11 --lib remote::replay_benchmark::tests
```
