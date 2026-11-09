# Offline software H.264 experiment

This developer-only experiment uses the **same production replay generator** as
`replay-benchmark`; it does not capture a screen, connect a socket, inject input,
probe a GPU, change the production codec, or enable a new stream profile.
Production transport remains JPEG tiles. The system FFmpeg/libx264 executable
must already be installed. The script does not download dependencies.

## Export and run

Build the existing helper using the usual release build for meaningful timings:

```sh
cargo build --release --locked --no-default-features --features remote-x11 --bin jwm-remote
target/release/jwm-remote replay-fixture \
  --output /explicit/workspace/replay-fixture \
  --width 320 --height 180 --frames 12 --seed 1 --quality 75
python3 scripts/benchmark-software-h264.py \
  --fixture /explicit/workspace/replay-fixture \
  --output /explicit/workspace/h264-run --trials 2 --warmup-runs 1
```

Both output directories must be new; parents must already exist. Export rejects
an existing destination and creates RGB files with `create_new`. It writes its
manifest last. A failure can leave explicitly owned partial artifacts; it never
removes input data or silently retries into an old directory. Inspect incomplete
output before removing it yourself. The Python benchmark deletes only its own
intermediate segment/encoded/decoded files on successful measurement, retaining
all run records and stderr logs.

The exporter directly calls `generate`, not a translated Python approximation.
It records each frame's scene/index, exact dimensions, geometry epoch and RGB24
SHA-256, then runs the production tile encoder/decoder with the same configuration
and fresh state. The fixture's producer label is not a signature or verified
build provenance. Preserve the reviewed source revision and build mode separately.
The script checks all files, hashes and geometry schedules, rejects input-leaf
symlinks and path traversal, and rechecks hashes when consuming a segment.
Intermediate RGB files are created exclusively. These checks do not provide
isolation against malicious replacement of shared parent directories; use a
workspace directory you control, not an adversarial shared directory.

## Scope and limits

The original replay validation still applies (up to 1920×1080, 300 frames per
scene, 300 million generated pixels). Export is tighter: no warmup and **64 MiB
aggregate RGB across all five scenes**. The benchmark accepts at most 25 contiguous
geometry segments, three measured trials and one discarded warmup run. Each
FFmpeg child has a 30-second wall/CPU timeout and 64 MiB per-file size limit.
Successful diagnostics are additionally capped at 256 KiB; a failing child may
leave a larger log up to the per-file limit. This is not a 64 MiB directory quota. The
aggregate loop admits no new segment after 180 seconds; an in-progress segment
can exceed that deadline while finishing its bounded children. Inputs are read
with explicit size limits; only one segment is materialized at a time.

Scene generation and JPEG semantics are documented in `remote-replay-benchmark.md`.
Resize starts a fresh H.264 encoder/decoder for every geometry epoch, including
when dimensions return to an earlier size. Odd dimensions are padded to even
sizes for YUV420 and cropped back after decoding. No old reference frame crosses
that boundary.

H.264 parameters are fixed and retained verbatim in each record: libx264,
`ultrafast`, `zerolatency`, one thread, no B frames/lookahead, GOP 60, fixed CRF
(default 23), repeated headers and access-unit delimiters. There is no `-re`,
network pacing or GPU path. The process receives a regular RGB file and runs as
fast as possible. Protocols are restricted to `file,pipe`.

## What the results mean

`report.json` preserves:

- FFmpeg version/build configuration and every actual command;
- fixture manifest hash, exact replay configuration and production tile baseline;
- separate warmup and measured runs, each starting fresh codec processes;
- wall/user/system process times and encoded bytes per segment;
- every framecrc packet observation with monotonic time and **read batch ID**;
- first-packet observation (startup-inclusive), and all subsequent observation
  intervals, separately, rather than calling an overall average “frame latency”;
- per-frame decoded RGB MSE, PSNR, maximum channel error and changed-channel ratio.
  Exact equality is explicit (`lossless: true`, null/infinite PSNR avoided).

**Packet observations are not encoder timings.** Muxer/pipe buffering, scheduler
wakeups and Python parsing can batch packets. Equal timestamps mean a shared read
batch, not zero-cost encoding. The first observation includes process startup,
input probing, conversion and encoding. Later intervals are observable output
cadence, not proven steady-state encode latency. No p95/p99 encode-latency claim
is made from these values. For true per-stage latency, a later in-process adapter
must timestamp actual input acceptance/output availability and retain PTS mapping.

CPU totals are per child invocation; `ru_maxrss` is explicitly the cumulative
maximum among this script's exited children, **not a per-run RSS measurement**.
Decode process timing includes startup and RGB output/materialization. The
existing tile baseline retains raw microsecond samples plus its own stage semantics and summary quantiles; those
are not directly comparable with FFmpeg subprocess wall times. Warmup invocation
primes code/caches only; measured codec processes start fresh.

JPEG quality 75 and H.264 CRF 23 are **not matched visual quality**. JPEG baseline
bytes include tile headers/bitmaps; H.264 bytes include Annex-B/SPS/PPS but no
transport framing. Idle JPEG suppresses frames whereas this H.264 experiment
encodes the whole offered sequence. Compare scene totals and measured distortion,
not bare ratios claimed as a general speedup. RGB PSNR is not a complete text
legibility metric; SSIM, text-edge review, matched-quality sweeps and representative
user-approved desktop clips remain future work.

A debug-build smoke run proves only that export/encode/decode/validation completes.
Software codec results do not establish NVIDIA capture speed, NVENC availability,
GPU-copy cost, network latency, actual presentation or physical input-to-visible
latency. Listing `h264_nvenc` in FFmpeg is not evidence of usable NVIDIA hardware.

Pure script self-test (no subprocess/files/sockets):

```sh
python3 scripts/benchmark-software-h264.py --self-test
```
