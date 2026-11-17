# Bounded vertical-band scroll-copy: offline A/B evidence

This is a single-pass debug-build CPU experiment, not a desktop, GPU, capture, network, DRM, two-machine or physical-presentation latency measurement. It does not establish production speedups. Scroll-copy remains explicit opt-in; the codec capability does not grant input permission.

## Method

Environment: Debian 13, x86_64 cloud CPU (reported AMD EPYC 9V74), Rust 1.99.1. Backend: offline CPU codec; renderer API, GPU driver, monitor topology and refresh rate are not involved. Production `tile.rs` Git blob: `757b9f5027d6f9d7b09003dc6b5181c42e405cd9`; workload Git blob: `bb3417880e1a9eb4955e467a2f5fdb8c8e75a8a5`.

Both arms use production `TileEncoder` and `TileDecoder`, quality 75 and identical deterministic source RGB. Each scene offers eight frames, normally 321 × 193; `odd_resize` switches to 303 × 181 after frame four. The ordinary arm disables scroll-copy, the second enables it. Input generation, hashing and distortion computation are outside codec timings; decode image materialization is also excluded. Each frame is generated in streaming fashion, with no stored RGB replay history. Both arms together remain below the 64 MiB offered-source RGB budget. Hashes agree for all 96 frame pairs (192 arm records) and all 12 scene pairs.

Values below include keyframes and suppressed frames: suppressed frames send zero bytes but retain decoded-canvas distortion. MSE is total squared channel error divided by channel samples, not a frame-average approximation. Positive percentages mean *more* bytes or *more* error. The near-half case uses rows [40,144), seven-pixel shifts, and 97 overlap rows, exactly ceil(193/2). Narrow-region changes affect only columns [32,200) and are unsupported. Repeated-static rows with a 32 × 16 changing patch must not choose a copy without a strict dirty-tile reduction.

## Bytes and distortion

| Scene | Bytes off → on | Bytes change | MSE off → on | MSE change | Worst channel error off → on | Copy frames |
|---|---:|---:|---:|---:|---:|---:|
| `static` | 56004 → 56004 | +0.00% | 230.97731 → 230.97731 | +0.00% | 118 → 118 | 0 |
| `up_7px` | 448827 → 106281 | -76.32% | 231.30315 → 230.95062 | -0.15% | 118 → 118 | 7 |
| `down_7px` | 448418 → 92406 | -79.39% | 231.37526 → 233.60609 | +0.96% | 104 → 104 | 7 |
| `random_change` | 449610 → 449610 | +0.00% | 231.03998 → 231.03998 | +0.00% | 118 → 118 | 0 |
| `repeated_static_local_change` | 33992 → 33992 | +0.00% | 29.71903 → 29.71903 | +0.00% | 22 → 22 | 0 |
| `fixed_toolbar` | 417077 → 137644 | -67.00% | 232.17857 → 231.52597 | -0.28% | 118 → 118 | 7 |
| `fixed_toolbar_down` | 417179 → 92401 | -77.85% | 232.25379 → 233.69243 | +0.62% | 100 → 91 | 7 |
| `header_footer_up` | 372058 → 124300 | -66.59% | 233.19691 → 233.01046 | -0.08% | 118 → 118 | 7 |
| `header_footer_down` | 372750 → 123891 | -66.76% | 233.93495 → 234.51982 | +0.25% | 100 → 91 | 7 |
| `near_half_band` | 278971 → 124179 | -55.49% | 232.17118 → 231.49682 | -0.29% | 118 → 118 | 7 |
| `unsupported_narrow_region` | 267812 → 267812 | +0.00% | 231.72606 → 231.72606 | +0.00% | 118 → 118 | 0 |
| `odd_resize` | 104704 → 104704 | +0.00% | 230.85955 → 230.85955 | +0.00% | 118 → 118 | 0 |

Copy reuses already decoded pixels rather than re-quantizing the overlapping region. It can retain earlier JPEG error and therefore **does not guarantee lower MSE or worst-channel error**. The table includes deteriorations without filtering.

## Interpretation

The new band scenes save 55.49%–77.85% of encoded bytes on this fixture, with seven copy frames each. Static, random, repeated-static/local-change, unsupported narrow-region and resize cases send exactly the same bytes and have exactly the same distortion in both arms. This does not make fallback free: random-change planning took 6199.3 µs versus 485.2 µs across eight offered frames, and narrow-region planning took 7410.2 µs versus 1133.2 µs. Those are observed sample costs, not a stable performance ratio.

Quality is not uniformly better: global down-scroll MSE rises 0.96%, fixed-toolbar down-scroll 0.62%, and header/footer down-scroll 0.25%. These results support retaining **opt-in**, not enabling the feature by default or promising universal quality/performance gains.

## Measured CPU work

These are the sums of raw elapsed microsecond samples from one debug run. They are sensitive to scheduling, cache state and arm order (ordinary first), so they are not latency claims or statistically supported speedup percentages. Planning overhead in fallback scenes is included. Null encode/decode samples in the full data mean a suppressed frame, not a timed zero-cost operation.

| Scene | Plan µs off → on | Encode µs off → on | Decode µs off → on | Commit µs off → on |
|---|---:|---:|---:|---:|
| `static` | 1510.0 → 1259.6 | 23508.8 → 21039.8 | 45564.7 → 40628.6 | 90.8 → 35.5 |
| `up_7px` | 534.4 → 7247.3 | 177204.0 → 46952.8 | 333880.8 → 89127.3 | 1629.4 → 526.1 |
| `down_7px` | 611.7 → 6623.9 | 187580.8 → 37005.9 | 334820.7 → 71454.6 | 1741.7 → 431.2 |
| `random_change` | 485.2 → 6199.3 | 179390.9 → 185859.4 | 338613.2 → 338456.6 | 1778.6 → 1962.9 |
| `repeated_static_local_change` | 1261.8 → 1469.3 | 19044.8 → 18494.6 | 41558.5 → 40634.5 | 40.7 → 45.8 |
| `fixed_toolbar` | 707.6 → 6327.6 | 161964.6 → 53677.1 | 313592.5 → 107943.8 | 1661.9 → 502.5 |
| `fixed_toolbar_down` | 724.4 → 6376.7 | 171855.1 → 33782.7 | 309730.5 → 68103.8 | 1599.4 → 444.4 |
| `header_footer_up` | 835.7 → 6484.5 | 136916.9 → 44736.5 | 265526.7 → 89784.2 | 1326.1 → 434.6 |
| `header_footer_down` | 843.9 → 7766.2 | 146592.4 → 44542.8 | 266757.3 → 88810.7 | 1548.7 → 516.5 |
| `near_half_band` | 1088.0 → 7770.9 | 105801.4 → 50382.8 | 202710.1 → 101517.9 | 946.4 → 466.7 |
| `unsupported_narrow_region` | 1133.2 → 7410.2 | 101543.1 → 102757.8 | 195432.8 → 198393.6 | 896.7 → 1113.4 |
| `odd_resize` | 1296.3 → 1828.7 | 40557.2 → 41508.5 | 75152.9 → 81400.0 | 51.9 → 57.9 |

[Complete per-frame and per-scene JSON](remote-scroll-band-benchmark.json) retains all CPU samples, payload sizes, SSE, sample counts, worst errors, source hashes and aggregate totals. No raw source pixels or credentials are included.

## Reproduce

```sh
cargo test --locked --no-default-features --features remote-x11 --lib \
  tile_scroll_copy_offline_ab_benchmark -- --ignored --nocapture --test-threads=1
```

The ignored test asserts positive copy use in supported scenes and zero copies in fallback scenes. It also enforces the offered-RGB budget and identical inputs across arms. This run used the bounded full-width band codec candidate, not an independent benchmark-only implementation.
