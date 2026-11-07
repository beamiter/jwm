# Remote-control optimization implementation

Baseline: `c8cf77ec` (2026-11-06). This first increment implements the
measurement, scheduling and resource-reuse foundations of the shared plan.
It is not completion of the entire remote-control roadmap.

## Implemented

- Capture-start to ACK latency now includes capture. Capture-complete mailbox
  age remains separate. Host `--stats-json` emits bounded-window quantiles and
  stage timing; see `remote-control.md` for exact semantics.
- Quality repair reuses a single pixel-damage scan even when ACK processing
  changes the quality epoch before encoding.
- Viewer network writes have a single owner outside the UI loop. Control and
  clipboard queues have count and byte limits. Key/button edges retain order,
  release fences discard stale input, and shutdown has reserved capacity.
  A bounded record deadline closes a stalled transport.
- Clipboard ingress and outbox are bounded. Local copies supersede pending old
  copies, compatible cumulative ACKs coalesce, overload fails explicitly, and
  shutdown cannot wait behind a full queue.
- CPU scaling caches geometry weights and scratch storage and no longer creates
  OS threads per frame. This implementation is serial; a persistent parallel
  pool is deferred until controlled benchmarks establish a useful thread budget.
- Capture RGB allocations return through an ownership-based bounded recycler on
  completion, supersession, error and shutdown.
- Native-size viewer frames carry damage metadata relative to the exact decoded
  predecessor. An unchanged keepalive reuses the completed backing; local changes
  convert and upload only their bounding rectangle. A skipped predecessor,
  changed geometry, resized window or scaled presentation uses the full snapshot.
  The full snapshot fallback preserves deltas even when the mailbox drops frames.
  Existing SHM completion and frame-ACK ordering are preserved.

## Explicitly deferred

- Asynchronous SHM double/triple buffering and patch-only decode-to-display
  handoff. The decoder still copies a full RGB snapshot, and SHM uploads still
  wait for completion. The native-size partial path reduces conversion/upload
  bytes, but does not eliminate all full-frame work.
- Scaled incremental upload, multiple merged damage rectangles, cursor plane,
  protocol negotiation/initial viewport, session restore/source switching,
  XKB/IME and clipboard UX modes, local readback, Wayland and hardware encoding.
- Separate atlas packing/JPEG timings, input-to-visible instrumentation,
  automated hardware metadata capture, and a fixed workload replay harness.

## Verification commands

```sh
cargo fmt --all -- --check
cargo check --locked --no-default-features --features remote-x11 --all-targets
cargo test --locked --no-default-features --features remote-x11 --lib remote::
cargo test --locked --no-default-features --features remote-x11 --bin jwm-remote
cargo clippy --locked --no-default-features --features remote-x11 --lib --bin jwm-remote --no-deps -- -D warnings
```

Native linking needs installed XCB and xkbcommon libraries. Full default-backend
validation additionally needs the development packages listed in CONTRIBUTING.
Unit tests do not establish real X11 presentation timing or hardware correctness.
Before deployment, compare identical idle/input/scroll/resize/file-transfer
workloads with and without these changes, including constrained bandwidth,
connection loss, Intel/AMD/NVIDIA, and a two-hour resource soak. Do not infer
latency or throughput improvements solely from passing unit tests.
