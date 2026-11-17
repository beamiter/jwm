# Remote workload baseline protocol

This protocol separates **repeatable workload preparation**, **host pipeline
telemetry**, and **physical input-to-display measurement**. The scripts and
fixtures can be tested without a desktop. That does not constitute a dual-machine
benchmark, RTX 5080 validation, Isaac Sim compatibility, or latency acceptance.

## Record before measuring

Use two physical machines, a trusted LAN, and the same clean build for each
comparison. Keep a private run manifest with:

- Source and viewer CPU/GPU, OS/kernel, GPU driver, display server/backend,
  monitor resolution/scaling/refresh rate, and power mode.
- Commit, build profile, host/client options, capture path (X11 desktop or window),
  source/window and encoded dimensions, frame rate, codec/quality, and whether
  the viewer was fullscreen. Record any dimension changes instead of silently
  comparing different resolutions.
- Wired/wireless link, measured available bandwidth and RTT, packet-loss profile,
  application version, workload/scene revision, and exact actions/duration.
- Run ID, warm-up duration, start/end procedure, and whether other processes
  competed for CPU/GPU/network. Record viewer performance separately: host
  telemetry cannot explain every viewer bottleneck.

`python3 scripts/remote-benchmark-metadata.py` provides optional read-only CPU
and GPU inventory; it does not measure GPU utilization or performance. Inspect
its CLI help for privacy controls. Never include credentials or private desktop
content in public evidence. Raw stderr and screenshots can contain private data.

Use one process session/build/workload per log. Warm up for at least 30 seconds,
then capture a fixed 60-second measurement interval, repeated three times.
Exclude warm-up *whole records* consistently; do not prorate a five-second
telemetry window. Record actual retained duration. If restarting changes warm-up
state, keep the session alive and select the steady-state records offline.

## Workloads

### Terminal

Open an actual terminal on the source, hold size/font/scaling fixed, then run:

```sh
python3 scripts/remote-terminal-workload.py --seconds 60 --fps 30 --rows 24 --columns 80
```

This bounded fixture alternates full repaint content, ASCII thin strokes,
Chinese and Greek text. `columns` is a text-character budget, not a guarantee of
terminal-cell width for wide Unicode. The fixture restores cursor/alternate
screen on normal exit and interruption. It requires a TTY and is not a terminal
emulator benchmark. Also measure a separate idle text phase; do not mix idle
compression savings into scrolling frame-rate results.

### Browser

Open `tests/fixtures/remote-workloads/browser.html` locally in the source browser.
It is self-contained and makes no network requests. Measure separate static,
30-second scrolling/motion, and modal-open/close phases. Fix viewport, browser
zoom, and refresh rate. Repeated keys toggle a black/white marker for external
camera work. DOM key handling and animation callbacks are not physical latency
measurements. Include real browser address-bar/menu/focus behavior separately;
this fixture alone cannot validate popup/window capture integration.

### Isaac Sim

Run the actual supported Isaac Sim version on the target GPU with a fixed scene,
seed, viewport resolution, simulation rate and render settings. Record launch,
steady simulation, orbit/pan, and UI/menu interaction as distinct phases. Save a
scene identifier/hash and action sequence so the next run can repeat it. Do not
replace this with the browser animation or claim cloud fixture tests validate
Vulkan presentation, GPU synchronization/readback, or NVIDIA drivers. If the
hardware/application is unavailable, mark this row **not measured**.

For each workload, take native-resolution source/viewer screenshots after at
least 500 ms of stillness and inspect text edges, color, thin lines, stale tiles,
clipping, and transient/modal contents. Preserve the exact encode settings.
Automated image comparison is useful only with aligned frames and known scaling.

## Collect and analyze host telemetry

Enable `--stats-json` on the documented `jwm-remote host` invocation and save its
stderr privately. See [remote-control.md](remote-control.md#machine-readable-pipeline-timings)
for the wire-compatible telemetry schema and stage definitions. For example,
after collecting a steady-state log:

```sh
python3 scripts/remote-workload-report.py \
  --telemetry terminal-run1.stderr --workload terminal --run-id terminal-run1 \
  > terminal-run1.report.json
```

The offline parser uses Python's standard library. It accepts bounded schema-1
host records, ignores non-JSON stderr diagnostics without copying them into the
report, and rejects malformed JSON objects, unknown timing policies, mixed
builds, non-finite/negative measurements and excessive input. Use a fresh output
path: shell redirection truncates an existing file even when validation fails.

Report interpretation:

- Throughput uses the sum of actual `window_seconds`, including partial windows.
  Payload Mbit/s excludes transport overhead and is not measured link capacity.
- Weighted means are computed only when the schema exposes the matching
  observation count: capture/captured, writer and socket/sent, ACK/drawn ACKs.
  Other whole-run means remain null instead of inventing denominators.
- Stage quantiles cover the **most recent 128 observations per stage per window**.
  `max_reported_window_recent_p95_ms` is the largest such window p95, not a
  whole-run p95. Whole-run percentiles remain null. Archive raw observations if
  a later instrumentation version supports exact run-level percentiles.
- Stages overlap and have different denominators. Do not sum their means.
  Capture-start-to-ACK includes capture and viewer drawing; send-to-ACK also
  includes viewer work. Neither is network RTT or physical input-to-display.
- GPU fences, GPU-to-CPU readback and encoder-copy costs are not independently
  exposed by this schema. Use separate platform traces for those subdivisions.

## Physical latency and acceptance

Use an external high-speed camera framing the physical input trigger and viewer
response, or an independently calibrated input/photodiode apparatus. Document
what marks physical actuation (for example a hardware-trigger LED), scanout
location, camera frame rate/exposure, frame-count method and uncertainty. A
browser timestamp, injected key event, frame ACK, or screenshot is not a valid
substitute. At 240 fps, one frame is about 4.17 ms; disclose quantization and do
not report precision finer than the measurement supports.

Take at least 100 samples per trial and three independent trials for each
workload/configuration. Retain failures and outliers; do not select only the
fastest actions. Create an operator-measured CSV with this exact header:

```csv
trial,sample,latency_ms
```

Rows use non-personal trial IDs, unique positive sample IDs per trial, and finite
nonnegative measured milliseconds. There are deliberately no example measurement
values to confuse with real results. Add `--physical-csv measured.csv` to the
report command. It reports nearest-rank per-trial p95, whether sampling reached
3 × 100, and whether each measured p95 is at most the plan's 50 ms target.
`measurement_method_verified` stays false: a CSV parser cannot certify the
apparatus, measurement method or end-to-end acceptance. An undersampled CSV is
explicitly flagged even if all its numbers meet the target.

Maintain a result matrix for terminal, browser and Isaac Sim with: source/viewer
configuration, FPS, payload rate, capture/encode/write/ACK timing, physical p95
and uncertainty, static image quality, interaction failures, and evidence paths.
Mark missing measurements **not measured**, not zero or pass. Compare competitors
only under the same hardware, workload, resolution and measurement procedure.
Network degradation tests require a separately authorized network setup; these
scripts do not alter system/network settings. Repeat the matrix under each
agreed bandwidth/RTT/loss profile rather than extrapolating from the wired LAN.

## Cloud regression checks

```sh
python3 scripts/test-remote-workload-report.py
python3 -O scripts/test-remote-workload-report.py
python3 scripts/test-remote-benchmark-metadata.py
```

These checks cover fixture generation, bounded parsing, aggregation, missing
sample handling and physical-CSV bookkeeping. They do not run real two-machine
capture, a browser renderer, Isaac Sim, or a high-speed camera. Physical baseline
rows remain pending until their evidence is collected on the target machines.
