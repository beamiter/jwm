# Remote upgrade terminal trials

Two physical machines ran eight SSH-forwarded 1080p JPEG trials with private
Xvfb displays. The host drew the repository terminal workload in GNOME Terminal
3.44.0. The viewer used the X11 display backend. Host and viewer both used a
release build with `remote-x11,remote-wayland,remote-h264`.

| Role | Machine | OS and kernel |
| --- | --- | --- |
| Host | Core i7-13650HX, RTX 5060 Laptop GPU, NVIDIA 580.178.04 | Ubuntu 22.04.5, 6.8.0-149-generic |
| Viewer | Core Ultra 7 270K Plus, RTX 5080, NVIDIA 595.71.05 | Linux Mint 22.3, 6.17.0-35-generic |

The baseline was `0631f2639676e6dc1cc4d1c81cdad8d0a2804e17`. The staged
candidate was `3086bbaeda7dad8fe40e475dc435a85843e8f8d6` with
`JWM_REMOTE_CURSOR_WORKER=1` on the host and `JWM_REMOTE_READ_AHEAD=1` on the
viewer. The candidate predates the regional readback, adaptive bitrate and
recovery window changes.

Both arms used a 1920×1080 source, encoded frame and viewer window. The host used
root capture, fixed JPEG quality 85 and a 30 FPS ceiling. The viewer requested
JPEG and view-only mode. The source ran the following in a private GNOME
Terminal window with geometry `128x42+0+0`.

```sh
python3 scripts/remote-terminal-workload.py --seconds 115 --fps 30 --rows 42 --columns 126
```

Discard the first six full telemetry windows and retain the following twelve.
The measurement used two ABBA blocks. Each raw window retains counters and
recent-sample quantiles. The typed windows and derived reports are included
here. [Telemetry window archive](telemetry-windows.tar.gz) · [Reports](reports.json)

| Trial | Arm | Retained s | Sent FPS | Payload Mbit/s | Capture max window p95 ms | Capture-to-drawn-ACK max window p95 ms |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | Baseline | 60.073 | 27.650 | 2.393 | 4.518 | 102.727 |
| 2 | Candidate | 60.060 | 27.739 | 2.400 | 4.436 | 61.190 |
| 3 | Candidate | 60.090 | 27.708 | 2.398 | 4.444 | 105.410 |
| 4 | Baseline | 60.092 | 27.658 | 2.394 | 4.298 | 94.446 |
| 5 | Baseline | 60.079 | 27.614 | 2.390 | 4.308 | 105.557 |
| 6 | Candidate | 60.063 | 27.704 | 2.397 | 4.646 | 97.821 |
| 7 | Candidate | 60.105 | 27.718 | 2.399 | 3.928 | 105.304 |
| 8 | Baseline | 60.093 | 27.557 | 2.385 | 4.569 | 152.569 |

The four candidate trials paired host and viewer windows by session ID.
Viewer decode queue max window p95 ranged from 0.208 to 0.239 ms. The
baseline viewer emitted human-readable counters. Per-stage viewer quantiles
were unavailable for baseline comparison.

Sent FPS includes repeated frames. The p95 columns take the highest recent
window p95 within each trial. The ACK times varied enough that these trials
give an inconclusive latency comparison. Other engineering work ran during
the trials. Eight ICMP samples collected afterward ranged from 6.344 to
209.621 ms. Link capacity and physical input-to-display latency were
unmeasured.

A separate two-machine smoke test on the candidate used JPEG, H.264 and HEVC
4:4:4 over an SSH tunnel. Each mode presented frames, delivered cursor updates,
switched to the granted 640×360 region and exited normally. The hardware runs
recorded actual downloads on the viewer. The smoke used private Xvfb screens
with synthetic motion. GPU-native presentation and physical display timing
require separate evidence.

## Visible terminal scrolling

Three later trials measured real XTerm scrolling on Mint and the Ubuntu
viewer at 1920×1080 JPEG Q85. The workload scrolled one row at a time in
a 192×54 character grid. The endpoint observers read a checksummed
sequence marker and the glyph row separately. Each static or motion phase
used 30 seconds of warmup and 60 seconds of observation.

Both endpoints remained unchanged in all three static phases. The source
displayed 60.00 distinct updates per second in each motion phase. The
viewer displayed 23.70, 22.28 and 23.05 updates per second, with all three
below 27 FPS. The viewer reported no invalid or reordered markers. The
motion gap p95 ranged from 188.93 to 211.43 ms, and the longest gap was
980.08 ms. The host's maximum recent-window JPEG p95 ranged from 102.83
to 111.77 ms, while the corresponding viewer decode values ranged from
13.39 to 13.67 ms. The archive retains the three earlier fixture startup
failures caused by a missing import, the corrected workload and the raw
software raster and telemetry records.
[Scored XTerm trials](x11-terminal-scroll-scored.json) ·
[Raster records, endpoint windows and replay](x11-terminal-scroll-scored-raw.zip)

A short `remote-fast-jpeg` diagnostic kept the same XTerm workload, JPEG
quality and Ubuntu viewer. Each phase used 5 seconds of warmup and 15
seconds of observation. The source reached 60.00 distinct updates per
second. The viewer reached 24.33, with a 166.57 ms motion gap p95 and a
313.23 ms longest gap. Static marker and glyph samples stayed unchanged.
An early static crop matched all 33,416 thresholded foreground pixels in
the 1,200×200 text region. Per-channel RGB mean absolute error was 1.65.
The largest host JPEG p95 across recent telemetry windows was 71.85 ms.
The pilot missed the 27 FPS gate, so the three formal pairs did not run.
[Fast JPEG diagnostic](x11-terminal-scroll-fast-jpeg.json) ·
[Raster evidence, build pins and replay](x11-terminal-scroll-fast-jpeg-raw.zip)

The scroll-copy detector now ranks displacements using full-width row
samples. The old detector's three sampled columns were blank in the XTerm
rasters. Replay matched the archived static and motion-end PPMs byte for
byte, then modeled 34 transitions at each of 20, 40 and 60 pixels. The new
detector found the correct displacement in all 102 transitions. The 20 and
60 pixel transitions met the existing copy eligibility and reduced the
modeled repair area. The 40 pixel transitions stayed below the dirty-tile
threshold. The sparse-column Rust regression failed before the fix and
passed afterward. The focused suite passed 154 tests with two explicit
ignores. [Detector replay and regression result](x11-scroll-copy-detector.json) ·
[Replay source and test logs](x11-scroll-copy-detector-raw.zip)

Three subsequent pairs compared the old and revised detector in AB, BA, AB
order. Both builds used `remote-x11,remote-fast-jpeg` and `--scroll-copy`
with JPEG Q85. Each 1080p XTerm phase used 30 seconds of warmup and 60 seconds
of scoring on Mint and the pinned Ubuntu viewer. Motion at the source was
60.00 FPS in every trial. Static marker and glyph samples stayed unchanged
in all six trials. The early static crops had 100% foreground agreement
within the selected text region.

| Pair | Old viewer FPS | Revised viewer FPS | Old motion-gap p95 ms | Revised motion-gap p95 ms | Old longest gap ms | Revised longest gap ms |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 25.13 | 28.45 | 167.34 | 44.79 | 199.04 | 1,685.96 |
| 2 | 23.07 | 28.23 | 166.86 | 55.16 | 3,744.38 | 855.76 |
| 3 | 24.37 | 28.82 | 177.68 | 55.27 | 411.11 | 278.02 |

The revised detector passed the 27 FPS cadence gate in each pair. Host
telemetry recorded 85, 84 and 89 successfully flushed copy frames in the
revised motion windows, and zero with the old detector. Those reporting
windows include the telemetry tail. The FPS and gap estimates use exactly
the 60-second raster intervals. In pair 1, the revised viewer read the same
pixels 152 times during the longest hold. Source motion continued and the
host reached both frame credits. In pair 2, the old host exhausted both
credits and its send-to-ACK p95 reached 3.66 seconds in the corresponding
host-local window.
The aggregate records leave the cause of the delivery or feedback delay
unresolved. The pilot, six scored trials and the first pair's gap analysis
were replayed separately from the published archive.
[Scored detector comparison](x11-terminal-scroll-copy-detector.json) ·
[Source pins, samples and replayers](x11-terminal-scroll-copy-detector-raw.zip)

A separate revised-detector run sampled the owned SSH and Ubuntu viewer
sockets every 250 ms. It recorded 28.72 viewer FPS and a 455.68 ms longest
visible gap. The 40 reads inside that gap showed one marker and glyph
signature. Two Ubuntu samples inside the gap showed 393,216 additional
received bytes on the viewer's loopback connection. Mint reported 14
retransmissions in the broader host sequence neighborhood. Each endpoint
contributed 240 scored socket samples using its own monotonic clock.
These observations leave the cause of the gap unresolved.
[Passive socket diagnostic](x11-terminal-scroll-copy-tcp-diagnostic.json) ·
[Sanitized observations and offline replay](x11-terminal-scroll-copy-tcp-diagnostic-raw.zip)

A single diagnostic used the same XTerm workload and endpoint builds with
hardware H.264. It displayed 28.78 FPS and a 44.22 ms viewer motion gap
p95. During the static interval, the sequence marker stayed fixed and the
viewer glyph signature switched 28 times between two states. An early
static 1,200×200 text crop retained 92.58% of the source's thresholded
foreground pixels, with 96.20% precision. The trial exceeded the cadence
threshold and did not meet the combined static and motion condition.
[Hardware H.264 pilot](x11-terminal-scroll-h264-pilot.json) ·
[Raster evidence and replay](x11-terminal-scroll-h264-pilot-raw.zip)

A single HEVC 4:4:4 diagnostic used actual NVENC encoding and NVDEC
decoding with the same workload. It also displayed 28.78 FPS. The viewer
motion gap p95 was 44.09 ms. The static crop's foreground mask matched
the source at the selected threshold. Over the full static interval,
the viewer sampled the alternate glyph signature 173 times in 28
bursts while the source stayed unchanged. The combined condition
remains unqualified.

The source screenshots at the start and end of each hardware static
phase were byte-identical. The selected static host windows still
contained 865 H.264 frames and 1,676 HEVC frames. In this capture path
the encoder can refine identical input when all display credit has
been returned. The viewer's median interval between alternate glyph
bursts was 4.10 seconds for H.264 and 2.09 seconds for HEVC. These
endpoint-local records do not identify the packet that started an
individual burst.
[Hardware HEVC 4:4:4 pilot](x11-terminal-scroll-hevc444-pilot.json) ·
[Raster evidence and replay](x11-terminal-scroll-hevc444-pilot-raw.zip)

A separate HEVC 4:4:4 lossless device preflight encoded 81 pictures per
input on Mint's RTX 5080. Ubuntu's RTX 5060 Laptop decoded each stream
through NVDEC and downloaded every visible Y, U and V plane. All 81
decoded pictures matched the canonical YUV input in each of the 1,200×240
XTerm crop, 1,920×1,080 XTerm and 256×256 chroma tests. The full XTerm
stream's largest access unit was 520,973 bytes. The checker verified the
initial, scheduled and forced IDRs and the 1 MiB access unit limit. An
initial 128×96 chroma test hit the encoder's minimum size. This preflight
used the platform FFmpeg CLI on each endpoint.
[Lossless device preflight](hevc444-lossless-device-preflight.json) ·
[Bitstreams, plane hashes and replayer](hevc444-lossless-device-preflight-raw.zip)

A matched 1080p XTerm pair then used the release HEVC 4:4:4 option with
the same Mint NVENC host and Ubuntu NVDEC viewer in both arms. Each static
and motion phase warmed up for 30 seconds and scored 60 seconds. The two
sessions stayed on the negotiated hardware codec and exited normally.

| Retained observation | CBR | Lossless constant QP 0 |
| --- | ---: | ---: |
| Static viewer glyph samples matching the source | 5,243 / 5,400 | 5,400 / 5,400 |
| Visible unique motion FPS | 28.70 | 28.50 |
| Visible motion gap p95 | 44.33 ms | 44.40 ms |
| Longest visible motion gap | 504.02 ms | 444.31 ms |
| Static video payload | 1.479 Mbit/s | 1.879 Mbit/s |
| Motion video payload | 1.695 Mbit/s | 2.555 Mbit/s |

The pair also scored a separate 10-second interval after the viewer first
observed the final scroll marker. The selected glyph mask matched at that
first read in both arms. The CBR viewer then diverged in 29 of 900 reads.
Its last uninterrupted run of matching samples began 8.59 seconds after
the marker. The lossless viewer matched in all 900 reads. The interval is
measured on the viewer's local clock and covers the selected glyph mask.
[XTerm lossless comparison](x11-terminal-scroll-hevc444-lossless-option-pair.json) ·
[Raster, codec telemetry and replay](x11-terminal-scroll-hevc444-lossless-option-pair-raw.zip)

## Follow-up checks

- The native `ssh --stats-json` command forwarded parseable host and viewer
  records from a private 800×600 Xvfb session on the Ubuntu host. The records
  carried the same authenticated session ID.
- On both machines the runtime preflight encoded test frames with NVENC,
  decoded H.264 with VA-API and HEVC 4:4:4 with NVDEC, and imported the VA-API
  H.264 surface as two DMA-BUF layers into EGL on `/dev/dri/renderD128`.
  The import probe ends at EGL image creation.
- On the local machine the loopback fault proxy interrupted the X11
  connection. The same viewer window and 930×530 client area survived a
  successful reconnect. Closing the viewer during the retry delay cancelled
  the next attempt. In a separate replay the proxy accepted the replacement
  TCP connection and withheld its authentication greeting. An ordinary X11
  window close took 79.13 and 85.84 ms to exit cleanly in two candidate
  runs. The older viewer was still running after the 2-second fixture deadline.
  [Retained-window recovery results](x11-viewer-recovery-close.json) ·
  [Fault traces and fixture](x11-viewer-recovery-close-raw.zip)
- An isolated file-enabled X11 viewer recovered when the proxy sent a FIN
  to the viewer and kept receiving traffic in the other direction. The
  recovered session confirmed a restriction to text and images. A second
  directional FIN returned a transport EOF while a retry remained available.
  The listener stayed live and admitted exactly two authenticated sessions.
  An earlier simultaneous cut of both proxy pumps caused a broken pipe
  during cleanup, so the viewer stopped at the first failure.
  [Clipboard recovery and cleanup evidence](x11-viewer-recovery-close.json)
- The X11 window safety workflow runs all four recovery cases using two
  private Xvfb displays per case and a loopback fault proxy. It exercises
  retained window geometry, close during retry delay, close during stalled
  authentication and the confirmed clipboard restriction. The local replay
  used the workflow's build features and passed all four cases. The window
  close during stalled authentication took 76.62 ms, and 77.56 ms on the
  subsequent build that passed formatting and both X11 Clippy commands.
  The subsequent build also passed 71 CLI tests and preserved the `doctor`
  options and combined probe output.
  [Remote window safety workflow](../../../.github/workflows/remote-window-safety.yml) ·
  [Local replay result](remote-feature-matrix.json)
- The read-ahead regression used authenticated loopback TCP and the same frame
  size and geometry epoch for both sources. The old source accepted its JPEG
  delta. The same delta was rejected after the source barrier, and a keyframe
  from the new source was accepted. [Regression code](../../../src/remote/client.rs)
- Native Wayland view-only read-ahead passed the real CLI source-switch case
  with the option off and on. The case read the replacement role's blue
  buffer while holding its feedback, then delivered late positive feedback
  on the retired red role before confirming the new selection. It returned
  to the first grant on the same TCP session. The 11-case CLI suite passed.
  A separate Intel VA-API and EGL run dequeued 398 frames with read-ahead
  enabled. It retained native DMA-BUF presentation across resize and source
  switch. On the same second source it exercised RGB presentation of H.264
  after an import rejection, followed by JPEG after GPU capture was withdrawn.
  The six shared X11 queue and source-barrier tests passed separately.
  [Read-ahead results](native-wayland-read-ahead.json) ·
  [CLI traces and source pins](native-wayland-read-ahead-raw.zip)
- In an isolated X11 H.264 session, a viewer-only gate held the decoder
  inside one real libswscale call. A host XTEST target observed the next
  tap's press and release in 1.34 ms and 10.01 ms. Closing the viewer
  released a held key at the host in 3.17 ms. A separate X11 client obtained
  both local grabs before the codec gate opened. The viewer and host then
  exited normally. These are software event timings from one loopback run.
  [Slow decode result](x11-read-ahead-slow-decode.json) ·
  [Fixture and raw events](x11-read-ahead-slow-decode-raw.zip)
- A paired follow-up held a real H.264 conversion for 2.55 seconds in each
  arm with both video credits in use. At 2.244 seconds the old release still
  held F8 on both X servers and owned both viewer grabs. The fixture then
  sent a key-up. The candidate autonomously cleared the source key at
  2.034 seconds while the viewer X server still showed F8 down. A separate
  X11 client acquired both grabs at 2.036 seconds, with the conversion still
  blocked. Both arms reported the terminal queue-age timeout after the gate
  was released. The first old attempt stopped during the mouse preflight and
  is retained separately.
  [Paired queue expiry result](x11-read-ahead-queue-expiry.json) ·
  [Raw events, logs and replay](x11-read-ahead-queue-expiry-raw.zip)
- Two authenticated JPEG sessions verified the viewer's new read timings
  through `connect --stats-json`. Each displayed seven frames and recorded
  six samples in each of the header, body and authentication stages. The
  read-ahead session also recorded six decoder-queue samples.
  [Viewer read timing results](x11-viewer-read-stages-smoke.json) ·
  [Raw viewer and host records](x11-viewer-read-stages-smoke-raw.zip)
- A Rust client used `RemoteSession` against an owned X11 host. It selected
  a granted region, presented the new source, rejected the old catalog's
  command, obtained a timing window, and disconnected.
- The controlled Wayland fixture used two private Wayland displays, software
  GL and authenticated loopback TCP. Both warm-up samples and all 16 measured
  samples reached the matching viewer presentation event. Injection to the
  observed presentation measured 40.06 ms p50 and 51.12 ms p95 in this one
  run. The source was 200×150 and the viewer 400×300. [Samples](wayland-input-visible.json)
- A release build on Ubuntu connected a native view-only viewer to GNOME
  Shell 42.9 through a private Wayland socket. The compositor ran in headless
  mode with a 1024×768 virtual monitor. Over authenticated loopback TCP, the
  viewer displayed and acknowledged all 79 JPEG frames in the two recorded
  five-second windows. The Xvfb source was 960×720 and the viewer was 704×528.
  A second run through `RemoteSession` switched to an authorized 640×480
  region on the same connection, rejected a command from the old catalog
  epoch, and disconnected cleanly.
  In a third run the compositor issued 15 window resizes. The viewer
  acknowledged 108 frames across three reporting windows and exited normally
  after the compositor closed the window.
  [Host and viewer telemetry](wayland-mutter-smoke.json)
- After the view-only resize and source-switch changes, the remote regression
  suite passed 1377 tests with 48 explicit ignores. All 68 CLI tests passed.
- Before the presentation-queue fix, the parallel Wayland lease-expiry test
  missed its expected PNG repair on both the working build and the pinned
  baseline. The updated queue presents a pending PNG repair before an
  immediately following native proof that shares the exact tile snapshot and
  scope. All six combined-control lossless integration tests passed together
  on the updated build, including natural expiry. The follow-up remote
  regression suite passed 1376 tests with 48 explicit ignores.
- The later feature matrix ran after the retained-window, read-ahead timeout
  and reused GPU backing fixes. X11 passed 1,104 remote library and 71 CLI
  tests, with 29 explicit library ignores. The Wayland CLI without hardware
  codecs passed 71 tests. The combined Wayland, Udev and hardware codec
  build passed 1,399 remote library tests with 65 explicit ignores. The
  hardware codec library and CLI also compiled without Wayland.
  [Feature matrix](remote-feature-matrix.json) ·
  [Raw logs, source digests and replay](remote-feature-matrix-raw.zip)

The 50 ms physical interaction target remains unmeasured. The test setup
has no high-speed camera or calibrated optical acquisition device.

## Clipboard input during an upload

Two private Xvfb displays on the development machine ran the release host
and viewer over authenticated TCP. A loopback proxy capped the viewer-to-host
direction at a nominal 10 Mbit/s. A synthetic 12 MiB file traveled in that
direction alongside keyboard input. The fixture timed each XTEST key release
at the viewer and its corresponding key-up event at the host using the same
monotonic clock. It collected 144 release samples per arm in three pairs.

| Viewer bulk settings | Release p95 during upload | Idle release p95 | Median file throughput |
| --- | ---: | ---: | ---: |
| Default 128 KiB chunks, 1 MiB window | 927.99 ms | 11.99 ms | 9.140 Mbit/s |
| `JWM_REMOTE_CLIPBOARD_BULK_KBIT=10000` | 74.91 ms | 12.17 ms | 9.079 Mbit/s |

All six files completed with matching SHA-256 and clipboard URIs. In six
additional runs, the proxy closed during an active upload and the host
released its held key, discarded the partial file and retired the incomplete
clipboard offer. Separate runs used the Pause escape binding to narrow the
clipboard to text and images during an upload. Both policies canceled the
file and removed staging, refused a later file, and still delivered text in
the same session. [Input and transfer samples](x11-clipboard-input-budget.json)

Two later ABBA blocks held the same nominal 10 Mbit/s uplink while
comparing clipboard budgets of 10,000 and 6,000 kbit/s. Each budget had
four complete uploads and 192 measured releases during active staging.
The smaller 18,750-byte chunks and 45,000-byte window reduced pooled
release p95 from 76.10 to 63.25 ms and median file throughput from
9.06 to 6.32 Mbit/s. All eight forced disconnects cleared the held key
and incomplete clipboard data. The separate restriction trial at each
budget canceled file transfer and preserved permitted text.
[Clipboard budget ABBA](x11-clipboard-budget-abba.json) ·
[Input edges, telemetry and replay fixture](x11-clipboard-budget-abba-raw.zip)

Four further trials triggered Ctrl+Super+F12 after the host confirmed a
held key and a partial 12 MiB upload. Each trial verified that the host
released the key and the viewer freed both X11 grabs before the local key
went up. Reacquisition delivered a fresh key press and release in order.
The original authenticated session completed the same upload, and the
receiver's file SHA-256 and clipboard URI matched.

The shortcut-to-host-release samples on the nominal 10 Mbit/s uplink
were 910.80 and 911.06 ms with the default clipboard budget, and 58.20 and
57.23 ms with the 6,000 kbit/s budget. File throughput was 9.16–9.19 and
6.29–6.31 Mbit/s, respectively. These four software samples cover the
shaped upload scenario.
[Release during upload](x11-clipboard-release-all-diagnostic.json) ·
[Input edges, transfer evidence and replay](x11-clipboard-release-all-diagnostic-raw.zip)

The unshaped software timing runs used 1920×1080 private X11 displays, a
static JPEG source and clipboard disabled. Each session warmed up for
30 seconds and scheduled 100 shortcut releases over 60 seconds. On IPv4
loopback, the shortcut and host key-up observer shared a monotonic clock.
The three sessions each verified all 100 releases, with p95 of 2.46, 2.55
and 2.48 ms.

The Mint-to-Ubuntu route used a separate SSH connection to return the
host's observed key-up. Its viewer-clock elapsed time gives an upper
bound and includes the response trip. Two sessions verified all 100
releases, with upper-bound p95 of 6.26 and 6.40 ms. The third session
verified 60 of 100. Its observed subset had an upper-bound p95 of
137.19 ms. A further shortcut produced a host key-up reply in 30.80 ms
and timed out while waiting for the held-key state check. A total of 39
slots were unissued. The archive retains both unscored pilot failures,
including the pointer-only handshake before the two owned displays used
matching keyboard maps.
[Unshaped software timing](x11-release-all-software-latency.json) ·
[Slot ledgers, build pins and replay](x11-release-all-software-latency-raw.zip)

A separate three-session repeat returned the host's held-key check with
the key-up observation in a single message. Its viewer-clock upper bound
includes the host's X11 keymap query. Two sessions verified 100 releases
each, with p95 of 7.02 and 6.61 ms. The other session verified 84 releases.
Slot 85 timed out waiting for the observer to arm, before the shortcut
was issued. The arm response arrived 3.71 seconds after its request.
Fifteen slots were unissued. Both endpoints and the observer exited
normally. The failed slot and its late response remain in the archive.
[Key-up and state-check repeat](x11-release-all-software-latency-inline.json) ·
[Slot, teardown and replay evidence](x11-release-all-software-latency-inline-raw.zip)

A later repeat used the same pinned builds, inline keymap proof and
unshaped SSH route. Two sessions verified 100 releases each. Their
upper-bound p95 latencies were 7.03 and 6.91 ms, and their maxima were
7.88 and 82.82 ms. In the third session, warmup slot 29 timed out waiting
for the reacquisition edge. No scored shortcut was issued in that session.
The three actors exited normally and the 100 absent score slots remain in
the report. Across both inline batches, four complete sessions met the p95
threshold and two sessions were incomplete.
[Inline route recheck](x11-release-all-software-latency-route-recheck.json) ·
[Attempt ledgers, build pins and replay](x11-release-all-software-latency-route-recheck-raw.zip)

Replaying the two incomplete slots identified where each preparation
sequence stopped. In the scored arm timeout, the observer round trip was
3.71 s and the host read-to-reply interval was 0.32 ms. In the warmup
reacquisition timeout, the button-down observation arrived 984.15 ms after
issuance. The button-up observation arrived 398.15 ms after issuance and
109.74 ms after the slot expired. Teardown retained the actual button-up
edge. Both timeouts occurred before the viewer issued the release shortcut.

The next repeat pipelined both reacquisition edges and attached the host keymap
to the held-key press event. All six pilot releases completed. In scored runs,
session two verified 100 releases with a p95 upper bound of 5.32 ms. Session
one verified 60 releases before scored slot 61 timed out. Session three timed
out at warmup slot 28. Both timed out before the shortcut. The matched late
arm replies took 3.99 and 4.26 seconds round trip. Host read-to-reply time was
0.29 and 0.36 ms. The three-session qualification remains open.
[Pipelined setup results](x11-release-all-software-latency-pipelined-setup.json) ·
[Attempt ledgers and replay](x11-release-all-software-latency-pipelined-setup-raw.zip)

The source-click repeat opened each sample when the owned X11 receiver saw
the real button press and verified its empty keyboard state. The press and
release of the held key retained their inline keymap witnesses. Three formal
sessions returned all 100 release observations each in their exact 60-second
measured windows after 30 seconds of warmup. The replayed p95 upper bounds
were 4.21, 4.57 and 4.49 ms. The interval starts at the viewer's release
shortcut and ends when the viewer receives the independent SSH observation
of host key-up and cleared state.
[Qualified software results](x11-release-all-software-latency-source-click-setup.json) ·
[All 300 scored samples and replay](x11-release-all-software-latency-source-click-setup-raw.zip)

## X11 regional readback

Two physical machines used the same release build over authenticated TCP
forwarded by SSH. The Ubuntu host used a private 1920×1080 Xvfb display with
an X11 Composite overlay, independent cursor sampling and fixed-quality JPEG.
A deterministic painter changed one 104×74 rectangle during each timed run.
The arm changed only `JWM_REMOTE_X11_REGIONS`. Each trial discarded at least
30 seconds of warmup and retained at least 60 seconds of whole telemetry
windows. The viewer sampled both colors of the changing rectangle and three
unchanged areas.

Across two ABBA blocks, the four regional trials sent 1,918 JPEG frames.
The host reported 1,806 partial reads and an average of 128,333 planned
readback pixels per sent frame. Full-frame capture used 2,073,600 pixels per
frame, a 93.81% reduction in the regional arm. The largest reported window
capture p95 per trial ranged from 5.20 to 9.35 ms for regional capture and
8.47 to 9.88 ms for full-frame capture. Separate full-repaint phases in the
regional arm used 157 full reads and validated both expected pixel states.
One warmup ended with viewer I/O returning `EAGAIN`. The same-case retry
passed, and all eight retained trials closed normally.
[ABBA telemetry and pixel samples](x11-regional-readback-abba.json)

The private-Xvfb integration regression also passed with two separate sparse
captures while the host sender was blocked on its initial write. The second
capture left the older queued snapshot intact and replaced it in the mailbox.
The authenticated JPEG records decoded to the initial image and an updated
image containing both changes. [Regression code](../../../src/remote/x11_capture_regions.rs)

## Cursor during capture readback

A private X11 proxy held one core GetImage request on the host capture
connection. The fixture moved the source cursor between two known points
while the viewer kept the previous video frame. With independent cursor
sampling, the actual viewer pixels showed the cursor at both points before
the readback resumed. The capture-loop control kept the previous position
until the gate opened. Both runs then displayed the updated frame and
closed normally. [Pixel observations](x11-cursor-readback-smoke.json) ·
[Fixture and raw logs](x11-cursor-readback-raw.zip)

The follow-up used the same three-second capture gate for each cursor
mode. In both, normal key events reached the selected X11 receiver
during the hold. The viewer's grab-release chord cleared the source
keymap while the corresponding viewer key was still held. Source and
viewer focus stayed on the same windows. Source release was observed
9.00–10.19 ms after the chord with about 2.47 seconds left before the gate
opened. Both cases then displayed the new frame and cursor and closed
normally.
[Blocked-readback input result](x11-blocked-readback-input.json) ·
[X server observations and replay](x11-blocked-readback-input-raw.zip)

Another owned X11 test switched between two authorized 320×320 regions
on one authenticated session and one viewer window. The first core
read of region B was held for 1,502.60 ms. All six samples preserved
region A's last raster and frozen cursor sprite. Neither new pointer
position appeared while the read was held. After release, the viewer
showed region B and its cursor within 38.39 ms, then showed the next
B cursor move and cleared its previous point. Source and geometry
epochs advanced and both endpoints exited normally.
[Regional cursor switch result](x11-regional-cursor-source-barrier.json) ·
[Source barriers and pixel replay](x11-regional-cursor-source-barrier-raw.zip)

## NVENC rate adaptation pilot

Two-machine pilots used private 1920×1080 Xvfb displays and the H.264 NVENC
host path over SSH on the Ubuntu host's Wi-Fi connection. A proxy paced only the
forward stream at 400,000 bytes/s in the shaped lanes. Cropped X11 readback
counted unique source markers from the actual viewer drawable.

- With a 16,384-byte effective send buffer, the host observed credit pressure
  and reopened NVENC at 24 FPS and 100% nominal bitrate, from 30 FPS. The
  viewer observed 196 unique frames over 25.00 seconds of pilot windows.
- With 8,192-byte effective buffers on the host send and proxy receive ends,
  the host observed write pressure and reopened NVENC at 30 FPS and 85%
  nominal bitrate. The viewer observed 271 unique frames over 30.00 seconds.
- The uncapped fixed-rate control observed 588 unique frames over 20.00
  seconds, or 29.40 per second, with a 30 FPS request and a 60 FPS source.

Both adapted sessions retained the same authenticated session ID, H.264 codec
and geometry. These short pilots include warmup. Timed comparisons require
matched fixed-rate and adapted runs after warmup.
[Pilot scope, rate changes and telemetry](x11-nvenc-adaptive-rate-pilot.json)

A later release used application v36 to carry viewer durations in drawn
frame acknowledgements. Three sessions ran on private 1280×800 Xvfb
displays over authenticated loopback. Both the viewer-off and host-off
sessions used v35 and returned 143 drawn H.264 acknowledgements each.

The v36 session held real viewer RGB conversions for an additional 45 ms
with a private test shim. Four windows reported mean viewer decode of
57.19–57.76 ms. The host classified the pressure as viewer-side and
reopened NVENC at 24 FPS with the same nominal bitrate. After the shim
delay was removed, the governor reopened at 28 FPS and returned to the
requested 30 FPS. The v36 session recorded 1,866 matching drawn ACKs
under the same authenticated identity. Both endpoints exited normally.
This run verified the wire profiles and FPS decisions.
[Viewer feedback and rate transitions](x11-nvenc-viewer-feedback-recovery.json) ·
[Endpoint observations, fixture and replay](x11-nvenc-viewer-feedback-recovery-raw.zip)

### NVENC resolution changes and recovery

Four managed H.264 sessions used the RTX 5080 NVENC host, CPU scaling in
the X11 viewer, private 1280×800 displays and authenticated IPv4 loopback.
The viewer conversion shim held each RGB conversion for an extra 45 ms.
With the opt-in `motion` resolution policy, the host reached 24 FPS before
encoding 960×600 and then 640×400. The first build stayed at 640×400 after
removing the delay. Viewer mean decode fell below 4 ms while mean presentation
remained at 29.00–71.62 ms.

The corrected build used the same delay and restored 960×600 and then
1280×800 while holding 24 FPS. It then recovered to 28 and 30 FPS. The
viewer decoded and acknowledged each new geometry. Its pointer at
half-resolution arrived at the expected host pixel. A separate session
resized the viewer at the 75% tier, encoded at its new 1120×700 limit, and
then dropped to 840×525. Switching from that tier to an authorized 896×560
region restored the region's full size. The pointer included the region
origin within one pixel, and 78 subsequent drawable reads matched the region
marker. The opt-out session stayed at 1280×800 under the same delayed decode.

An injected failure of one NVENC submission at 960×600 selected JPEG at
the ordinary 1280×800 size on the active source. It delivered the expected
pixels and pointer position. Across the four corrected-build sessions,
both endpoints reported 6,759 drawn ACKs and exited normally. The peak
outstanding frame count was two. Formal workload quality and tail-latency
comparisons remain open.
[Resolution, draw and input results](x11-nvenc-adaptive-resolution.json) ·
[Endpoint records, raw drawable samples and replay](x11-nvenc-adaptive-resolution-raw.zip)

### Network and native-size comparisons

The write-pressure comparison completed two ABBA blocks. Each trial
retained 60 seconds of viewer observation after a 30-second warmup. Every
adaptive run applied both a bitrate and an FPS reduction in the same H.264
session.

| Write-pressure arm | Trials | Median unique FPS | Median trial gap p95 | Range of longest visible gaps | Median payload rate |
| --- | ---: | ---: | ---: | ---: | ---: |
| Fixed | 4 | 9.24 | 142.67 ms | 0.24–0.40 s | 2.884 Mbit/s |
| Adaptive | 4 | 9.12 | 143.09 ms | 0.47–3.56 s | 2.839 Mbit/s |

The 3.56-second gap included a regular host window with zero sent frames.
The host continued to capture while it waited for a video credit. It resumed
H.264 at the same geometry after the delayed acknowledgement. The precise part
of the delivery or ACK path that stalled is unresolved. The first uncapped
ABBA block cleared the 27 FPS gate for a 30 FPS request in all four trials.
All four trials in the second uncapped block fell below the gate. Across the
two blocks, the four fixed trials ranged from 11.88 to 28.75 unique FPS and
the four adaptive trials from 11.30 to 28.75. The host capture and encoder
continued at the same dimensions. Credit and acknowledgement waits grew in
the slower block. The runtime conditions varied across the uncapped trials,
so these runs establish the tested rate changes and frame timing distribution.
[Scored runs](x11-nvenc-adaptive-abba.json) ·
[Raw windows and observer samples](x11-nvenc-adaptive-abba-raw.zip)

An unscored repeat sampled both proxy directions and the owned SSH transport
socket without recording payloads. Its longest visible gap was 515 ms, and
no proxy send span or pacing timer overrun crossed 100 ms. The earlier
multi-second stall did not recur. [Transport trace](x11-nvenc-transport-trace.json)

The three fixed H.264 runs at 2560×1440 finished at 22.35–23.52 unique FPS
against a 27 FPS gate for the 30 FPS request. The three fixed 3840×2160 runs
finished at 8.25–12.70 unique FPS. Every run retained 60 seconds at the exact
source, encoded and viewer dimensions. At 3840×2160 the median reported
window p50 was 40.94 ms for capture, 24.93 ms for host RGB-to-YUV conversion,
and 29.73 ms for viewer YUV-to-RGB conversion. These are stage observations
from separate windows in a pipelined session.

Short 1440p and 4K pilots changed the host to Auto scaling. Both used MIT-SHM
readback at native size, with no core readback or CPU resize observations.
The 1440p pilot recorded 27.63 unique FPS over 30.00 seconds. The 4K pilot
recorded 13.43 unique FPS over 30.01 seconds, with a 370 ms longest visible
gap. The median capture window p50 was 4.94 ms at 1440p and 11.57 ms at 4K.
These pilots include warmup and confirm which capture path ran.
[Native-size results](x11-native-size-motion.json) ·
[Raw native-size telemetry and frames](x11-native-size-motion-raw.zip)

### Scored 1440p core and MIT-SHM readback

Two ABBA blocks used the same fixed H.264 NVENC host and the same RGB X11
viewer. Every trial retained 60 seconds of visible-frame observations after
30 seconds of warmup. Every active CPU capture window used core GetImage,
and every active Auto capture window used MIT-SHM. Both kept native 2560×1440
geometry with no resize observations.

| Host scaling | Trials | Unique FPS range | Median trial capture p50 | Worst visible gap |
| --- | ---: | ---: | ---: | ---: |
| CPU | 4 | 22.25–26.08 | 18.94 ms | 440 ms |
| Auto | 4 | 25.02–25.67 | 4.98 ms | 2.93 s |

The trial capture value is the median of its recorded window p50s. All eight
runs stayed below 27 unique FPS for the 30 FPS request.
[Scored results](x11-native-1440p-readback-abba.json) ·
[Raw samples and trial validator](x11-native-1440p-readback-abba-raw.zip)

### Scored 4K core and MIT-SHM readback

Two ABBA blocks switched only the Ubuntu host's scaling policy. All eight
trials kept 3840×2160 capture, fixed 30 FPS NVENC H.264 and a Mint VA-API
viewer that presented RGB through X11. The two machines used uncapped SSH
forwarding over Wi-Fi. Each trial discarded 30 seconds of warmup and scored
60 seconds of independent viewer samples. CPU scaling recorded 2,218 core
GetImage reads. Auto recorded 3,220 MIT-SHM reads. Neither used CPU resizing.

| Host scaling | Unique FPS range | Median trial capture p50 | Median trial gap p95 | Longest visible gap |
| --- | ---: | ---: | ---: | ---: |
| CPU | 7.78–12.17 | 89.46 ms | 246.35 ms | 435.06 ms |
| Auto | 12.97–13.27 | 11.54 ms | 144.87 ms | 565.06 ms |

The trial capture value is the median of its recorded window p50s. Auto
increased distinct FPS and recorded a longer maximum gap in every pair.
All eight trials stayed below 27 FPS. The observer scored visible marker
motion on private Xvfb drawables.
[Scored 4K results](x11-native-4k-readback-abba.json) ·
[Raw observations and offline validator](x11-native-4k-readback-abba-raw.zip)

### Native 4K color conversion and viewer upload pilots

Two short 3840×2160 comparisons kept the fixed 30 FPS NVENC setting and
core capture. Each enabled one experiment. The 4:2:0 run activated four
conversion workers on each endpoint. The SHM run recorded the 33,177,600-byte
upload buffer and its first direct RGB upload completion on the viewer.

| Experiment | Control unique FPS | Enabled unique FPS | Control longest gap | Enabled longest gap |
| --- | ---: | ---: | ---: | ---: |
| Parallel 4:2:0 conversion | 8.56 | 10.36 | 291 ms | 7.41 s |
| Direct RGB upload to MIT-SHM | 13.56 | 12.56 | 168 ms | 612 ms |

The median conversion-window p50 in the parallel run was 7.76 ms on the
host and 18.01 ms on the viewer, compared with 24.97 ms and 40.72 ms in
its control. During the 7.41-second visible gap, a host window captured 33
frames with two credits outstanding and sent zero frames. A viewer window
received and drew zero frames. The largest host window p99 was 7.57 seconds
from send to acknowledgement. These observations leave the exact source of
the stall unresolved.

Direct upload reported a median presentation-completion window p50 of 17.28
ms, compared with 20.45 ms in its control. Visible cadence fell in that pair.
The four runs include warmup and remain diagnostic. Both paths are opt-in.
The same 4:2:0 test compared active bytes with the whole-frame converter on
the two endpoints' different libswscale versions. The Xvfb regression checked
direct upload completion order, actual window RGB pixels and recovery after
an injected SHM request error.
[Pilot results and exact build digests](x11-native-4k-candidate-pilots.json) ·
[Raw telemetry, frame observations and tests](x11-native-4k-candidate-pilots-raw.zip)

An uncapped repeat sampled the owned SSH and viewer sockets on the viewer
clock. Each socket contributed 30 samples fully inside the observer window.
The longest visible gap was 266 ms, shorter than the one-second socket
sampling interval. The earlier multi-second stall did not recur.
[Passive socket trace](x11-native-4k-uncapped-transport-trace.json)

A subsequent viewer revision packed visible rows directly from the guarded
conversion bands into the recycled RGB frame. In a short pair with the same
Ubuntu host, MIT-SHM capture and parallel 4:2:0 on both endpoints, viewer
decode median window p50 was 48.57 ms on the earlier build and 41.98 ms on
the packed build. Unique FPS was 13.78 and 13.98 respectively. The recorded
longest gaps were 279 ms and 273 ms. The new viewer's conversion timing
includes its band copies. The earlier build recorded 417 separate full-frame
copy observations. The packed path recorded no such stage. Both runs include
warmup.
[Packed viewer pilot](x11-native-4k-packed-viewer-pilot.json) ·
[Raw frame observations and telemetry](x11-native-4k-packed-viewer-pilot-raw.zip)

The Intel viewer also decoded the pinned 256×256 H.264 probe through
VA-API and read the same surface through a direct CPU NV12 map and the
existing download API. Both access orders matched all active bytes across
the luma and chroma planes. After copying into aligned conversion buffers,
the full RGB output and a 251×247 crop also matched byte for byte. This
checks that device and probe frame. The test recorded no timings.
[Direct NV12 map compatibility](vaapi-cpu-nv12-direct-map-preflight.json) ·
[Probe source and replay](vaapi-cpu-nv12-direct-map-preflight-raw.zip)

A later check used two single-IDR 3840×2160 fixtures with SMPTE 170M limited
range and BT.709 full range. Both access orders matched all 12,441,600
active NV12 bytes per frame. Each comparison also matched the full RGB
frame and the 3839×2159 crop after conversion from aligned staging.
The mapped planes had 3,840-byte strides. FFmpeg supplied no plane-buffer
extent for those mappings. The check measured pixel compatibility.
[4K compatibility report](vaapi-cpu-nv12-direct-map-4k.json) ·
[Pinned packets, probe and raw records](vaapi-cpu-nv12-direct-map-4k-raw.zip)

Two further ABBA blocks held Ubuntu MIT-SHM capture, fixed H.264 NVENC and
the same Intel X11 RGB viewer build at 3840×2160. Only the viewer's direct
map setting changed. Each trial scored 60 seconds after 30 seconds of
warmup. The default arm recorded 3,300 downloads. The direct map arm
recorded 3,521 maps into padded NV12 staging and zero fallbacks.

| Viewer readback | Median visible FPS | Median trial gap p95 | Longest visible gap |
| --- | ---: | ---: | ---: |
| VA-API download | 13.25 | 140.17 ms | 526.12 ms |
| Direct NV12 map with staging | 14.58 | 138.52 ms | 590.48 ms |

The median of viewer decode window p95s was 85.93 ms for download and
82.23 ms for direct map. All four pairs gained distinct FPS and recorded a
longer maximum gap with direct map. Every trial stayed below 27 FPS. The
timed path used SSH over Wi-Fi and software observations of X11 drawables.
[Scored direct map comparison](x11-native-4k-vaapi-direct-map-abba.json) ·
[Pinned build evidence, observations and replay](x11-native-4k-vaapi-direct-map-abba-raw.zip)

A separate Intel VA-API video-processing probe tried a packed RGB download
at 3840×2160. The device rejected BGR0 for both pinned H.264 pictures.
BGRA downloaded 33,177,600 bytes per picture with fully opaque alpha.
Against the NV12-to-CPU-RGB reference, the maximum channel difference in
guarded solid patches was 2 for SMPTE 170M limited range and 43 for BT.709
full range. The full-range picture's mean absolute channel difference was
11.31. The probe measured decoded pixels and recorded no throughput.
[Packed RGB probe](vaapi-vpp-packed-rgb-4k.json) ·
[Both output attempts and offline replay](vaapi-vpp-packed-rgb-4k-raw.zip)

### X11 CPU presentation preparation

An owned MIT-SHM test fed identical decoded RGB frames to synchronous
and prepared draws. The full drawable matched byte for byte at 326×241
and 258×190, including asymmetric letterboxing and Triangle downscaling.
A window resize and return to the old dimensions advanced its viewport
revision. Drawing the earlier result used the original RGB path. Each
draw waited for its own upload and copy completion.

A separate H.264 session test viewed an odd 1393×785 source on private X11
displays. The enabled run used 634 prepared frames and completed a source
switch, held-key release and fresh input on the new source. One disabled
run completed the same checks. Another timed out waiting for a click
after the resize burst. All captured black bars and defined color probes
matched. Later full rasters varied by up to five RGB channel levels,
including differences between the two disabled runs.
[Session correctness diagnostic](x11-cpu-prepare-correctness-diagnostic.json) ·
[Captured RGB, failure record and replay](x11-cpu-prepare-correctness-diagnostic-raw.zip)

A follow-up waited for zero outstanding host frame credits at the restored
1393×785 geometry before injecting the click. Both arms then completed
resize, source selection and held-key release. The enabled arm submitted
797 prepared frames. Three capture points matched byte for byte. The initial
independently encoded rasters differed by up to four channel levels at
570 of 1,093,505 image pixels. The controller's pixel-check exit is retained.
[Restored-geometry retry](x11-cpu-prepare-correctness-drained-retry.json) ·
[Retry captures, input edges and replay](x11-cpu-prepare-correctness-drained-retry-raw.zip)

An initial two-machine 3840×2160 view-only marker pair used one viewer
binary, fixed H.264 and host MIT-SHM. Each arm scored 60 seconds after
30 seconds of warmup. With preparation enabled, the median presenter
window p95 fell from 24.16 to 6.93 ms. Mean decoded-ready to consumed X11
completion rose from 24.41 to 36.68 ms over the matched draws. Visible
FPS changed from 13.10 to 12.37. The enabled viewer used 754 prepared
frames in retained windows.
[4K scored pair](x11-native-4k-presenter-preparation-pair.json) ·
[Endpoint telemetry, build pins and replay](x11-native-4k-presenter-preparation-pair-raw.zip)

The follow-up used two OFF–ON–ON–OFF blocks on the same two machines.
All eight trials retained a full 60 seconds and 12 telemetry windows
per endpoint after warmup. The four enabled trials recorded 3,061
prepared submissions in their retained windows, with zero fallbacks
or stale results.

| Measure | Synchronous | Prepared |
| --- | ---: | ---: |
| Presenter call, median window p95 | 24.78 ms | 6.90 ms |
| Decoded-ready to consumed X11 completion, mean per draw | 25.81 ms | 36.34 ms |
| Visible unique FPS, median trial | 13.26 | 12.73 |
| Visible gap p95, median trial | 138.60 ms | 138.70 ms |
| Longest visible gap across trials | 535.74 ms | 1,076.64 ms |

The presenter call shortened in every paired comparison. Decoded-ready to
completion increased in all four pairs because the work moved into time
spent waiting for submission. Paired visible FPS changes ranged from
−1.67 to +0.50. No trial reached the 27 FPS gate. The preparation path
remains opt-in.
[4K ABBA result](x11-native-4k-presenter-preparation-abba.json) ·
[Eight trials, endpoint telemetry and replay](x11-native-4k-presenter-preparation-abba-raw.zip)

One separate pair ran NVENC and VA-API over loopback on the same Mint
machine while scoring 4K motion. Each arm delivered all 100 shortcut
releases to the source application's X11 window. From local shortcut
injection to the source-observed key release, p95 fell from 22.19 to
9.51 ms and the maximum fell from 32.52 to 21.38 ms. The upper bound
including return through the local observation pipe had p95 of 22.42
and 9.65 ms. The source presented 30 FPS in both arms and the viewer
showed 18.82 and 19.05 unique FPS. The enabled arm recorded 1,056
prepared submissions in matching interior telemetry windows. This pair
measures software input handling with concurrent motion on one machine.
[ReleaseAll and motion pilot](x11-4k-presenter-preparation-releaseall-pilot.json) ·
[Input edges, visible markers, telemetry and replay](x11-4k-presenter-preparation-releaseall-pilot-raw.zip)

### Sustained 1080p resource use

The two machines ran a fixed 30 FPS H.264 NVENC session at 1920×1080 over
an uncapped SSH forward. After 60 seconds of burn-in, both endpoints
completed 900 measured seconds with 180 process samples each at five-second
intervals. Both exited normally from the same authenticated session.

| Endpoint | Sampled RSS peak | Last-minute median RSS minus first minute | Peak threads | Peak open descriptors |
| --- | ---: | ---: | ---: | ---: |
| Ubuntu host | 239,340 KiB | +376 KiB | 10 | 45 |
| Local viewer | 126,288 KiB | +48 KiB | 6 | 30 |

Thread and descriptor counts were constant in the retained samples. The host
reported a peak of two in-flight video credits. It sent and acknowledged
25,703 H.264 frames across 179 complete telemetry windows. The viewer drew
25,679 frames across its own 179 complete windows. RSS covers the two JWM
processes. GPU memory and viewer-visible unique FPS were unmeasured in this run.
[Soak result](x11-nvenc-resource-soak.json) ·
[Raw samples, telemetry and fixture](x11-nvenc-resource-soak-raw.zip)

### Sustained exact 4K over SSH

The same pinned Ubuntu NVENC host and Mint VA-API viewer ran a second
soak with 3840×2160 source and encoded scope and a 3840×2160 viewer window
request. The host used fixed H.264 with a 30 FPS request. The viewer used
CPU RGB presentation on an owned Xvfb display. Both processes retained
180 resource samples in 900 seconds after a 60-second burn-in and shut
down cleanly.

| Endpoint | RSS range KiB | First-minute median KiB | Last-minute median KiB | Threads | File descriptors |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ubuntu host | 447,196–511,968 | 479,464 | 479,566 | 10 | 45 |
| Mint viewer | 293,052–319,888 | 295,246 | 319,448 | 6 | 31 |

The viewer's RSS moved between levels about 24 MiB apart. From second 230
through the final sample it stayed within 319,172–319,888 KiB. Threads and
descriptors were constant. The host's peak reported outstanding credit
was two. The viewer returned 7,174 drawn ACKs across 895.35 seconds of
retained telemetry, or 8.01 per second. This configuration fell below the
requested 30 FPS. In the retained stage samples, core XGetImage averaged
105.07 ms. Viewer download, conversion and X11 completion averaged 21.37,
43.96 and 27.67 ms respectively. The soak measures endpoint process
resources and authenticated display acknowledgments.
[Exact-4K soak report](x11-nvenc-resource-soak-4k.json) ·
[Raw resources, telemetry and replay](x11-nvenc-resource-soak-4k-raw.zip)

## Native X11 presentation

A release build on the Ubuntu machine used an owned 1001×711 Xvfb capture
source, NVENC H.264 and authenticated loopback TCP. The viewer used the
Intel X11 DRI3 display and `/dev/dri/renderD128`. Four window samples
matched the synthetic source colors. The viewer window resized from
1393×1504 to 2560×1600 and switched to a granted 641×359 region. The
session recorded 312 DRI3 frame completions and zero CPU downloads.

In a second run the same valid node was selected with automatic scaling.
The mixed-driver XRender policy rejected the native path, so the viewer
downloaded the decoded picture and continued over RGB. It recorded 173
hardware downloads, presented the second source and closed normally. The
resize was performed in the native run. These short runs establish which
path ran.
[Host and viewer telemetry](native-x11-smoke.json)

The separate `doctor --probe-gpu-present` check rendered a 251×247 crop
from a 256×256 NV12 source and matched four color samples against an
independent CPU decode of the same H.264 packet.

The development machine's two isolated 3840×2160 Xwayland preflights did
not expose a matching Intel DRI3 device. Weston first selected the NVIDIA
render node. The Mesa-only retry rendered on Intel but still advertised the
NVIDIA node, and Xwayland disabled glamor and DRI3. These attempts stopped
before starting a video session.
[Preflight results](x11-private-dri3-preflight.json) ·
[Raw compositor logs and display checks](x11-private-dri3-preflight-raw.zip)

A private Smithay compositor then advertised the Intel DMA-BUF device
explicitly. Xwayland opened a 3840×2160, depth-24 display. Its DRI3 request
returned the same Intel render node used by the compositor, which drew the
full-size root DMA-BUF. The production `doctor` imported the native pixmap
and matched four colors in a 251×247 crop. The owned compositor shut down
cleanly. This preflight checked display and device compatibility.
[Intel Xwayland preflight](x11-intel-xwayland-4k-preflight.json) ·
[Display proof, fixture and replay](x11-intel-xwayland-4k-preflight-raw.zip)

A two-machine 4K pair used that display, fixed H.264 NVENC, MIT-SHM
capture and parallel host YUV conversion. Each arm scored 60 seconds
after 30 seconds of warmup. Both drew every recorded viewer frame with
VA-API, EGL and DRI3 and recorded zero CPU downloads. The eager arm created
1,139 XRender Pictures in retained windows at 0.43 ms mean. The deferred
arm created zero in 1,030 imports at the exact 4K dimensions.

| Measure | Eager | Deferred |
| --- | ---: | ---: |
| Visible unique FPS | 20.58 | 18.77 |
| Visible gap p95 | 76.75 ms | 80.39 ms |
| Longest visible gap | 266.52 ms | 237.55 ms |
| RGB target allocation, mean | 11.69 ms | 14.50 ms |
| EGL fence wait, mean | 15.42 ms | 16.38 ms |
| Native preparation and submission, mean | 31.71 ms | 35.27 ms |

Both arms fell below the 27 FPS gate. This single pair establishes an
eliminated XRender operation and measures the relative size of the
remaining stages. Deferred allocation of the XRender Picture remains
opt-in.
[Native 4K picture comparison](x11-native-4k-lazy-picture-pair.json) ·
[Frames, stage telemetry and replay](x11-native-4k-lazy-picture-pair-raw.zip)

A separate native lifecycle check drew exact 4K with zero picture setups
in the initial window. With the owned host stopped, the retained frame
redrew at 1297×973 and back at 3840×2160. All six sampled letterbox points
were black. The second authorized source, 1920×1080, used XRender scaling
on the same authenticated session and recorded picture setup. The host
and viewer recorded 106 draws and matching acknowledgements with zero
downloads and exited normally. The outer observer exited 1 when its
Xlib connection closed after the X server.
[Lifecycle diagnostic](x11-native-lazy-picture-lifecycle-diagnostic.json) ·
[Pixel samples, scopes and teardown record](x11-native-lazy-picture-lifecycle-diagnostic-raw.zip)

The next run closed the observer before the X server. It repeated the
same retained-frame resize and authorized source switch, returned 103
drawn acknowledgements and recorded zero downloads. The viewer, host,
display launcher and observer all exited normally.
[Clean lifecycle repeat](x11-native-lazy-picture-lifecycle.json) ·
[Replay and earlier observer failures](x11-native-lazy-picture-lifecycle-raw.zip)

An X11 RGB preallocation run repeated the lifecycle with one target
allocated ahead. It matched 140 worker allocations and two synchronous
allocations to 142 native imports, draws and host acknowledgements. It
checked colors and black bars after resize and restoration, switched to a
second source on the same session and recorded zero downloads. All owned
processes exited normally.
[Preallocation lifecycle](x11-native-prefetch-target-lifecycle.json) ·
[Pixel and allocation evidence with replay](x11-native-prefetch-target-lifecycle-raw.zip)

A two-machine 4K pair then kept eager XRender Picture creation enabled in
both arms. Each trial warmed up for 30 seconds and scored 60 seconds. The
enabled viewer used fresh worker-allocated buffers in all 1,571 retained
native imports, with two synchronous allocations around startup in the
whole session. Both arms recorded zero CPU downloads.

| Measure | Synchronous | Preallocated |
| --- | ---: | ---: |
| Visible unique FPS | 19.33 | 28.62 |
| Visible gap p95 | 79.29 ms | 42.37 ms |
| Longest visible gap | 370.07 ms | 158.27 ms |
| Native preparation and submission, mean | 34.15 ms | 17.93 ms |

The enabled arm met the 27 FPS gate in this pair. Allocating each used
target on the worker took 12.95 ms on average, outside the viewer's
preparation and submission duration.
[Native 4K preallocation pair](x11-native-4k-prefetch-target-pair.json) ·
[Source pins, observations and replay](x11-native-4k-prefetch-target-pair-raw.zip)

Two further OFF–ON–ON–OFF blocks used the same pinned build and path.
All eight trials retained 60 seconds of visible markers. All four enabled
trials reached 27 FPS, with 28.32–28.67 unique FPS. The four synchronous
trials recorded 18.65–19.55 FPS. In the retained viewer telemetry, all
6,303 enabled imports used fresh worker allocations and all 4,192 control
imports used synchronous allocations. The eight sessions recorded zero
CPU downloads.

| Measure | Synchronous | Preallocated |
| --- | ---: | ---: |
| Visible unique FPS, median trial | 18.93 | 28.61 |
| Visible gap p95, median trial | 79.49 ms | 41.39 ms |
| Longest gap between valid markers | 477.99 ms | 584.58 ms |
| Native preparation and submission, mean | 33.94 ms | 17.45 ms |

Each paired p95 gap improved. The longest gap increased in two pairs and
decreased in two. The trial with the longest gap also had 20 invalid
marker reads across the full observer run. The overlapping five-second
viewer window recorded a completion p99 of 20.82 ms. Only valid markers
contribute to visible FPS. The interval between valid markers remains in
the gap distribution. These records leave the cause of the interval
unresolved. Preallocation remains opt-in.
[Native 4K preallocation ABBA](x11-native-4k-prefetch-target-abba.json) ·
[Eight scored trials, telemetry and replay](x11-native-4k-prefetch-target-abba-raw.zip)

The next build routed initial presentation and receiver startup failures
through session cleanup. In two owned native X11 cases, the test overrode
the first real GLES completion with a timeout. Neither case acknowledged
the frame. Both joined the preallocation worker and retained the VA surface
through EGL teardown. A clean join kept JPEG retry eligibility. An injected
cleanup error prevented it.
[Initial submission failure checks](x11-native-first-submit-cleanup.json)

The combined remote regression run passed 1,409 tests with 71 explicit
ignores. Its one failure was an assertion matching the previous cleanup
function layout. After updating the assertion to check the new ordering,
the focused rerun passed. The X11 library build check and all 73 hardware
codec CLI tests passed with the updated assertion.

That build then ran a sustained 3840×2160 native X11 session with
preallocation enabled. The fixed H.264 NVENC host used MIT-SHM and parallel
YUV conversion. The Intel VA-API/EGL/DRI3 viewer used the same private
Xwayland setup over the two-machine SSH route. After a 60-second burn-in,
the independent drawable observer recorded 25,728 valid unique markers
over 900 seconds, or 28.59 FPS. The p95 gap was 41.52 ms and the longest
valid-marker gap was 523.46 ms. The observer recorded 34 invalid reads
across the full run. In complete telemetry windows within the retained
viewer interval, 25,482 of 25,484 native imports used worker allocations.
The viewer recorded zero CPU downloads. Each endpoint recorded drawn ACK
progress in every retained minute, and all owned processes exited normally.

| Endpoint | Valid 5 s samples | RSS range | Threads | Open file descriptors |
| --- | ---: | ---: | ---: | ---: |
| Viewer | 180 | 152,400 KiB | 15 | 40–42 |
| Host | 180 | 453,540–453,944 KiB | 14 | 46 |

Viewer RSS was unchanged across the sampled interval. The host's RSS rose
monotonically by 404 KiB and its first-to-last minute median rose by
376 KiB. The earlier 1080p host showed the same first-to-last minute
median increase. The sampled slopes in both runs were approximately
27 KiB/min with similar encoded frame rates. The cause of that drift is
open. The host observed a peak of two outstanding frame credits. The
archive reports the available Intel DRM counters per client and keeps
partial samples.
The NVIDIA host exported no standard DRM client memory counters. Each
endpoint uses its own monotonic clock for timing and resource samples.
[Native 4K resource soak](x11-native-4k-prefetch-resource-soak.json) ·
[Endpoint samples, source pins and replay](x11-native-4k-prefetch-resource-soak-raw.zip)

A same-host 1080p NVENC diagnostic then held capture, codec, nominal 30 FPS
and the viewer fixed while toggling only host JSON telemetry. Each arm
retained 240 seconds after a 60-second burn-in. The host sent 28.77 and
28.72 frames per second with the flag off and on, respectively. In both
arms the sampled RSS increased by 104 KiB, entirely in anonymous pages.
The slopes were 26.92 and 26.80 KiB/min. File and shared-memory RSS stayed
constant. Each arm kept 14 threads and 46 open file descriptors.

At the boundaries of each interval, `smaps` localized a 108 KiB rise to
one existing unnamed private mapping. The first setup attempt closed its
control pipe early and took no samples. Both arms of the corrected attempt
exited normally. Turning off JSON telemetry left the slope reproducible.
The allocation source and long-session limit remain open.
[Host memory attribution diagnostic](x11-nvenc-host-rss-attribution.json)

An independent FFmpeg process on Ubuntu reproduced similar growth with
synthetic 1920×1080 YUV420 input and output discarded. It loaded the
same libavcodec, libavutil, CUDA and NVIDIA encode libraries as the JWM
host. The owned process completed 7,196 frames in 240 seconds after
warmup. Anonymous RSS grew by 116 KiB between the retained boundaries,
and the measured slope was 28.66 KiB/min, about 16.3 bytes per encoded
frame. One existing anonymous private mapping accounted for 112 KiB of
the increase. The heap accounted for the other 4 KiB. File and shared
memory, thread and descriptor counts stayed constant. The growth therefore
reproduced in an encoder-only process. Identifying the allocation site and
whether the growth reaches a fixed limit still requires separate evidence.
[Encoder-only memory diagnostic](ffmpeg-nvenc-only-rss.json)

The one-hour encoder-only follow-up used the same pinned FFmpeg, CUDA and
NVENC libraries. After 60 seconds of warmup, the progress counter advanced
108,005 frames over 3,600 seconds. All 120 scheduled 30-second resource
samples and the retained-end sample were present. Anonymous RSS rose from
45,996 to 46,488 KiB. The final 86 observations held at 46,488 KiB from minute
17.5 through the retained end. File and shared-memory RSS stayed steady,
as did the 46 threads and 39 file descriptors.

An existing 5,616 KiB anonymous mapping accounted for 484 of the 492 KiB
rise. Its resident pages held at 800 KiB at the 30-, 45-, 55- and 60-minute
checkpoints. FFmpeg exited normally.
[One-hour encoder results](ffmpeg-nvenc-only-hour-soak.json) ·
[Resource samples, VMA ledgers and replay](ffmpeg-nvenc-only-hour-soak-raw.zip)

A separate FFmpeg run kept the library versions and traced memory syscalls
with user-space stacks. Between seconds 30 and 210, it encoded 5,390 frames
and gained 88 KiB of anonymous RSS. An existing 3,272 KiB mapping accounted
for 84 KiB of that increase. The trace associated all 818 pages of the
mapping with `mmap` calls during CUDA context initialization. The call stacks
include `avcodec_open2`, `cuCtxCreate_v2` and `__libc_calloc`. The trace
covers virtual mapping creation. The later RSS snapshots measure resident
pages in this separate process.
[Encoder mapping trace summary](ffmpeg-nvenc-mmap-origin.json)

The first one-hour 4K endpoint attempt verified the native viewer route and
finished warmup. The viewer then collected two scheduled retained samples.
The host reported two consecutive five-second windows with zero drawn
acknowledgments and exited with `Resource temporarily unavailable (os error
11)`. The viewer recorded a connection close. The incomplete archive retains
the endpoint windows, resource schedule and exit evidence.
[Incomplete 4K attempt](x11-native-4k-prefetch-resource-hour-soak-incomplete.json) ·
[Endpoint observations and exit evidence](x11-native-4k-prefetch-resource-hour-soak-incomplete-raw.zip)

The retry used the same pinned Ubuntu NVENC host and Intel VA-API/EGL/DRI3
viewer at 3840×2160 over SSH. After the 60-second burn-in, both endpoints
retained all 720 scheduled samples and exited normally. The viewer observed
102,849 distinct markers at 28.57 FPS. Every retained minute reached 27 FPS,
with a minimum of 27.77 FPS. The median and p95 marker gaps were 33.97 ms and
42.55 ms, and the longest gap was 924.07 ms. The viewer used preallocated
buffers for all 102,718 native imports in complete retained telemetry
windows. It recorded zero CPU downloads, and the host peaked at two
outstanding frame credits.

| Endpoint | Anonymous RSS at start | Anonymous RSS at end | Final steady interval | Threads | Open file descriptors |
| --- | ---: | ---: | ---: | ---: | ---: |
| Ubuntu host | 155,336 KiB | 155,816 KiB | 42 min 5 s | 14 | 46 |
| Mint viewer | 57,140 KiB | 57,272 KiB | 56 min 45 s | 15 | 40–42 |

The host and viewer mapping rollups at 30, 45 and 60 minutes agreed with
their respective plateaus. The report retains all available per-client Intel
DRM counters. The NVIDIA host exported no standard DRM client memory
counters. The two-second TCP samples advanced between successive readings
on both observed sockets. Each endpoint used its own monotonic clock.

The initial scorer rejected the viewer's valid zero shared-memory RSS.
The archive retains that exit and the original capture fixture. Its offline
replay accepts zero in the three RSS component counters and recomputes the
result from the same resource, pixel and TCP observations.
[One-hour native 4K results](x11-native-4k-prefetch-resource-hour-soak.json) ·
[Original capture and corrected offline replay](x11-native-4k-prefetch-resource-hour-soak-raw.zip)

A matched 1440p pair used the same native viewer build and host path.
Both arms captured, encoded and drew 2560×1440 at 1:1 inside the private
3840×2160 Xwayland root. Each arm scored 60 seconds after 30 seconds of
warmup. Both exceeded 27 unique FPS and logged zero CPU downloads. All
1,593 native imports in the enabled viewer's retained telemetry windows
used worker allocations.

| Measure | Synchronous | Preallocated |
| --- | ---: | ---: |
| Visible unique FPS | 28.80 | 28.98 |
| Visible gap p95 | 38.20 ms | 38.43 ms |
| Longest gap between valid markers | 541.47 ms | 290.14 ms |
| Native preparation and submission, mean | 16.76 ms | 10.71 ms |

The independent observer recorded one invalid read in each full run. Both
longest gaps fell inside the scored intervals. This pair measured lower
viewer-stage time with preallocation. The repeatability of the longest-gap
change is open. The Ubuntu Wi-Fi and Mint wired route matches the endpoint
metadata in the archive.
[1440p native preallocation pair](x11-native-1440p-viewport-prefetch-target-pair.json) ·
[Both trials, endpoint inventory and replay](x11-native-1440p-viewport-prefetch-target-pair-raw.zip)

Two 1440p OFF–ON–ON–OFF blocks followed at the same settings. Six of
the eight scheduled trials were eligible for scoring. The two remaining
control trials had ten of the required eleven complete viewer windows
inside the exact 60-second score interval. Their last otherwise complete
windows crossed the score boundary by 12.27 and 35.30 ms. Both rejections
remain in the archive, and the balanced comparison is incomplete.

All six qualifying trials exceeded 27 FPS. The median visible cadence was
28.85 FPS in the two control trials and 28.83 FPS in the four preallocation
trials. Mean native preparation and submission was 17.46 and 10.71 ms,
respectively. All 6,348 enabled imports in qualifying steady viewer windows
used worker allocations. The two complete adjacent pairs had mixed cadence
changes and shorter longest valid-marker gaps with preallocation enabled.
[1440p repeated preallocation results](x11-native-1440p-viewport-prefetch-target-abba.json) ·
[Eight attempts, window coverage and replay](x11-native-1440p-viewport-prefetch-target-abba-raw.zip)

A fresh two-block run started the observer about 2.5 seconds after a complete
viewer telemetry receipt. The two arms kept the same 30-second warmup,
60-second raster score and requirement for eleven complete viewer windows.
All eight attempts met the coverage and 27 FPS gates at the 2560×1440
viewport inside the same 3840×2160 compositor root.

| Measure | Synchronous | Preallocated |
| --- | ---: | ---: |
| Visible unique FPS, median trial | 28.825 | 28.833 |
| Visible gap p95, median trial | 38.43 ms | 38.00 ms |
| Longest gap between valid markers | 654.84 ms | 594.89 ms |
| Native preparation and submission, mean | 16.59 ms | 10.56 ms |

All 6,330 enabled imports in the retained windows used worker allocations.
The longest gap increased in three adjacent pairs and decreased in one.
The stage-time improvement repeated. Visible cadence was close to the request
in both arms, and the longest-gap changes were mixed.
[Aligned 1440p preallocation ABBA](x11-native-1440p-viewport-prefetch-target-aligned-abba.json) ·
[Eight scored trials, timing anchors and replay](x11-native-1440p-viewport-prefetch-target-aligned-abba-raw.zip)

The Ubuntu release build also passed `doctor --probe-gpu-encode` on
`/dev/dri/renderD128`. Direct libva PRIME_2 import retained the source's
layout and produced a 310-byte H.264 IDR through `h264_vaapi`. The IDR
decoded to the four expected colors. The source for this probe was
synthetic GPU storage.

A capture preflight on the same Ubuntu machine copied an owned X11 pixmap
to a 333×247 staging pixmap. DRI3 exported a single XRGB8888 DMA-BUF with
an Intel tiled modifier and 1536-byte stride. VA-API PRIME_2 imported it
on `/dev/dri/renderD128`. The system iHD `22.3.1+dfsg1-1ubuntu2` driver
failed to initialize video processing. An isolated test with iHD
`22.3.1+ds1-1ubuntu0.1` converted the same import to 352×256 NV12 and fed
that surface directly into `h264_vaapi`. The 214-byte IDR decoded to the
expected red-dominant Y/Cb/Cr sample (125, 91, 179).

### Static text and color over SSH

Three separate sessions sent the same 1280×800 private Xvfb source from the
RTX 5060 Laptop GPU machine to the development machine. The 1:1 X11 viewer
used VA-API for NVENC H.264 and NVDEC for NVENC HEVC 4:4:4. The third session
used JPEG at fixed quality 85. Each session authenticated, received drawn
ACKs and shut down cleanly. The test compared RGB pixels in the owned X11
displays after the first reporting window with a drawn ACK.

| Codec | Vertical 1 px chroma contrast retained | Horizontal 1 px chroma contrast retained | 13 px Chinese glyph mean absolute channel error | Gray patch maximum channel error |
| --- | ---: | ---: | ---: | ---: |
| H.264 4:2:0 | 0.26% | 0.26% | 12.16 | 2 |
| HEVC 4:4:4 | 98.97% | 99.20% | 1.43 | 1 |
| JPEG | 98.70% | 98.70% | 7.05 | 0 |

Contrast measures the change in red minus blue between alternating stripes.
Errors use 8-bit RGB channel values. The initial HEVC run retained 73.05%
of the vertical stripe contrast. The same loss was reproduced with a
software-only RGB to YUV444P round trip. Switching the two 4:4:4 conversions
from FFmpeg's fast bilinear mode to bilinear restored the contrast in the
repeat over SSH. The record includes both runs, 16 px and 22 px glyphs,
eight color patches and host and viewer frame counters.
[Static color samples](native-x11-codec-color-smoke.json)

An independent RTX 5080 hardware test verified a 641×359 visible crop and
checked the right edge, bottom edge and corner. Both codecs reported 642×360
in the SPS. H.264 encoded storage was 656×368 and HEVC 4:4:4 storage was
672×384. The decoded packed RGB crop matched the visible bytes of the padded
frame row for row. Both test packets used the BT.601 limited-range conversion.

At 641×359 visible pixels, the two explicit baseline RGB copy sites handle
1,381,794 bytes per frame. Packing just the visible rows handles 690,357 bytes
per frame. These are logical copy sizes calculated from the measured geometry
and the source sites. The separate 1080p soak observed 25,679 RGB pack
operations across 179 complete viewer windows. That geometry needs one
whole visible frame copy in both implementations.
[Decoder allocation sites and copy accounting](decoder-pool-copy-evidence.json)

Two subsequent 4K sessions recorded the new runtime counters on the same
viewer build. The whole-frame path reported 588 RGB materializations and
the banded path reported 499. Each started with two fresh RGB reserves and
one empty FFmpeg download target. Both reused the pool and download backing
for the remaining materializations. Each materialization copied 24,883,200
visible RGB bytes at its recorded pack site. Those counts cover the two short
sessions, including initialization and any final partial window.
[Runtime decoder work](x11-native-4k-decoder-rgb-work.json) ·
[Raw endpoint windows and verification](x11-native-4k-decoder-rgb-work-raw.zip)

The native NV12 presenter has a separate GLES pixel test for BT.601 and
BT.709, limited and full range, chroma sample locations and the visible crop
boundary. It uses generated textures in a surfaceless GL context.

## Native X11 host capture and encode

An opt-in release build used the Intel render node and isolated iHD driver
above for live H.264. The host captured from explicit rectangles on its
X11 Composite overlay, and an Xvfb viewer on the same Ubuntu machine
connected over authenticated loopback TCP. Two owned X11 windows covered
the red and green test regions.

The window resized to 626×480 while showing the 333×247 source. The
encoded dimensions changed from 268×200 to 332×246. Switching to the
authorized 281×215 green region produced 280×214 H.264. A third authorized
region measured 1×201 and triggered JPEG fallback in the same session.
The viewer kept the same window and acknowledged all 300 sent frames.
Host telemetry recorded 298 native DMA-BUF imports and H.264 encodes.
The remaining two frames were JPEG from the third region.
The same test with explicit root capture acknowledged 302 frames, including
300 native H.264 encodes. With the system iHD driver, native preflight failed
and ordinary CPU readback with NVENC remained available. A separate JPEG
control showed closely matching red pixels at the same sampled location.
[Switch, resize and fallback telemetry](native-x11-host-smoke.json)

## Native Wayland host capture and encode

Three trials on `ubuntu@192.168.0.78` used the Intel RPL-S render node and
the isolated iHD driver. Independent host and viewer CLI processes ran over
authenticated loopback TCP with private Wayland compositors. The host switched
from an authorized 200×150 red window to an authorized 240×180 blue window.
Both sources used linear XRGB8888 DMA-BUF capture, VA-API conversion and H.264
encoding. In every trial, the host recorded 198 native imports and the
viewer acknowledged 198 H.264 frames.

With the second window active, the source compositor withdrew its DMA-BUF
global. The host captured the same window through SHM and the viewer
acknowledged two fresh JPEG frames per trial. The session, source epoch,
geometry and viewer surface persisted through fallback. The fixture checked
every displayed pixel against the expected red or blue, including the last
row of both unaligned source sizes. It also checked for normal process exit.
The viewer used VA-API decode with CPU download and XRGB SHM. Physical KMS
scanout and input-to-display latency remain separate acceptance tests.
[Per-trial telemetry and pixel samples](native-wayland-host-smoke.json)

The private software GL source-switch fixture also exercised CPU scaling.
It verified both authorized source colors using a 120×90 encoded canvas.
The host reported 127 SHM readbacks, conversions and resizes, and two XRGB
staging capacity increases. It read 15,504,000 bytes from SHM. Applying the
pinned baseline's source sites to those observations gives 127 fresh XRGB
reserves, 127 fresh RGB destination reserves and 127 allocating resize
calls. The baseline counts are derived from the code and observed calls.
[CPU reuse result](native-wayland-cpu-reuse-smoke.json) ·
[Raw telemetry, source witnesses and tests](native-wayland-cpu-reuse-smoke-raw.zip)

## Native Wayland viewer presentation

The view-only H.264 viewer used Intel VA-API decode and GLES on the same
render node as its private Wayland compositor. The test inspected the
attached DMA-BUF and read back the rendered surface before returning
presentation feedback. Resizing to 481×320 preserved the red window and
its centered black borders. Source selection created a fresh viewer role
for the authorized blue window. Two trials retained separate telemetry
windows for each source epoch. Those windows recorded 397 native
presentations with zero hardware downloads.

The viewer compositor then returned a nonfatal DMA-BUF import rejection.
The same blue window and session continued through XRGB SHM while the
host kept sending H.264. Hardware download observations appeared in the
fallback windows. Both CLI processes exited successfully in each trial.
The test ran inside private compositors on the physical Intel GPU.
Physical scanout and input-to-display timing remain unmeasured.
[Per-source telemetry and pixel samples](native-wayland-viewer-smoke.json)

The expanded acceptance test verified capture, encode, decode and presentation
in one authenticated session on the same Intel render node. Each of the initial,
resize and source-switch reporting windows had 99 host DMA-BUF imports, zero
observed host CPU readbacks or RGB copies, and zero viewer hardware downloads.
Viewer import rejection left host GPU capture active. Withdrawing the source
compositor's DMA-BUF global then produced and acknowledged fresh JPEG frames
for the same blue source and geometry using the same viewer surface. The final
transition window contains the last H.264 frames as well as JPEG.
[Joined host and viewer evidence](native-wayland-gpu-chain-smoke.json)

The development machine's isolated Intel Wayland path also captured, encoded
and presented an exact 3840×2160 H.264 source end to end. The private
compositor read a 96×96 region of each actual rendered frame and verified four
asymmetric color samples and their orientation. It issued 51 monotonic
completions. The viewer window reported 50 native DMA-BUF presentations and
zero CPU downloads. Both endpoints reported drawn acknowledgements. The
source was static and synthetic.
[4K Intel result](native-wayland-4k-intel-smoke.json) ·
[Exact test and endpoint telemetry](native-wayland-4k-intel-smoke-raw.zip)

An eight-second motion preflight used the Ubuntu P09 X11 source, NVENC
H.264 and uncapped SSH. The owned Intel Wayland viewer presented 197
distinct 3840×2160 markers at 24.63 FPS. Its longest gap including the
observation boundaries was 56.61 ms. No marker read was invalid. The two
complete viewer windows and the final partial window reported native DMA-BUF
and zero hardware downloads. A shutdown snapshot repeated the last complete
window. The source painter submitted about 60 frames per second.
[4K motion preflight](native-wayland-4k-motion-preflight.json) ·
[Renderer observations and endpoint logs](native-wayland-4k-motion-preflight-raw.zip)

With the same pinned binaries and workload, three valid runs used 30 seconds
of warmup and a 60-second observation interval. The source submitted at about
60.01 FPS. The viewer showed 23.92, 24.10 and 23.82 distinct markers per second.
The motion gap p95 was 48.18–49.84 ms. The longest gap across the three runs
was 365.61 ms. All three ran below 27 FPS. Each retained 11 distinct full
viewer telemetry windows within its observation interval, covering
55.11–55.18 seconds. The viewer reported zero hardware downloads across all
session windows. One additional attempt ended during negotiation before its
observer started and remains in the raw evidence.
[Scored runs and negotiation failure](native-wayland-4k-motion-scored.json) ·
[Raw motion and telemetry](native-wayland-4k-motion-scored-raw.zip)

A separate phase diagnostic presented 412 distinct markers in 18 seconds,
or 22.89 FPS, after 10 seconds of warmup. All seven raw viewer windows
matched the managed session and reported zero hardware downloads. The two
whole windows inside the observation interval contained 229 observations per
stage. The median of the two window p50s was 16.3165 ms for EGL fence wait,
13.4985 ms for RGB target allocation, 4.252 ms for VA surface wait, and
0.9225 ms for Wayland DMA-BUF import.
[Phase diagnostic and matched windows](native-wayland-4k-phase-diagnostic.json) ·
[Raw endpoint evidence and fixture](native-wayland-4k-phase-diagnostic-raw.zip)

The opt-in RGB backing reuse preflight on the same native 4K path recorded
two RGB allocations across 792 native preparations. Two whole steady
windows contained 249 preparations and 249 new Wayland imports with no
allocations. The viewer reported zero hardware downloads and displayed
482 distinct markers in 18 seconds, or 26.78 FPS. This preflight was
shorter than the scored trials.
[Backing reuse preflight](native-wayland-4k-reuse-preflight.json) ·
[Raw evidence and observer replay](native-wayland-4k-reuse-preflight-raw.zip)

The scored comparison used the same release viewer, Ubuntu NVENC host,
marker workload and selected network route across two ABBA blocks. All
eight trials completed with 30 seconds of warmup and 60 seconds of
observation. Viewer median FPS was 24.05 with reuse disabled and 28.72 with
reuse enabled. All four reuse trials met the 27 FPS gate. The median of
per-trial motion gap p95s was 48.50 ms without reuse and 41.19 ms with reuse.
The longest observed gaps were 406.40 ms and 273.94 ms respectively.
All eight trials reported zero hardware downloads. The steady viewer
windows recorded no new RGB allocations in any reuse trial, with
1,473–1,582 fresh Wayland imports per trial.
[Scored backing reuse ABBA](native-wayland-4k-reuse-abba.json) ·
[Raw measurements and replay fixture](native-wayland-4k-reuse-abba-raw.zip)

The same pinned viewer passed two owned Intel hardware regressions with
backing reuse enabled. The static 4K test checked four differently colored
quadrants and recorded 51 compositor completions, 50 native viewer
presentations and zero hardware downloads. The second test verified
letterboxing after resize, correct pixels after a source switch,
viewer-side SHM recovery on rejected import, and fresh JPEG from the same
source after the source DMA-BUF capability was withdrawn. Both
endpoints closed normally.
[Hardware correctness results](native-wayland-4k-reuse-correctness.json) ·
[Endpoint telemetry and pixel assertions](native-wayland-4k-reuse-correctness-raw.zip)

The sustained diagnostic took one resource sample per second after a
30-second warmup. Across the 120 observation samples, viewer RSS was
187,536 KiB, its thread count was 13, and file descriptors ranged from
42 to 43. The two visible DRM clients reported resident memory within
64 KiB of their respective starting values. The session made two backing
allocations across 4,410 native preparations. The matched steady windows
contained 3,304 native preparations and zero allocations. This records
one private viewer process over the observed interval.
[Sustained resource diagnostic](native-wayland-4k-reuse-sustain.json) ·
[Procfs observations and replay](native-wayland-4k-reuse-sustain-raw.zip)

An owned Intel regression presented the first real H.264 DMA-BUF and
verified its pixels and ACK. A test-only hook then injected a timeout
result after the second real GLES fence wait completed. The viewer
retained the decoded surface through renderer teardown, reported the
underlying timeout, and terminated without a second commit or ACK.
The fault injection tests the timeout handling branch using real
buffers and a real compositor. Physical device loss remains unmeasured.
[Fence timeout result](native-wayland-gpu-fence-timeout.json) ·
[Test source and endpoint transcript](native-wayland-gpu-fence-timeout-raw.zip)

The opt-in reuse path also passed a fault test on an actual reused
target. Three healthy frames were presented and acknowledged. The
fourth draw reached a real signaled GLES fence, then the test hook
overrode its status with `GL_WAIT_FAILED`. The target returned to
neither the attached nor the idle buffer list. The failure produced
no new commit or ACK. The decoded AVFrame stayed owned through
destruction of the same EGL context. The same test build also passed
the original timeout case with reuse disabled. Both cases passed again
after the diagnostic was corrected to report session closure on
unresolved GPU work.
[Reused-target fence failure result](native-wayland-gpu-reused-fence-failure.json) ·
[Fault transcript and replay](native-wayland-gpu-reused-fence-failure-raw.zip)

## Mint and Ubuntu network route

A separate diagnostic used Mint's `eno1` Ethernet interface and Ubuntu's
`wlp3s0` Wi-Fi interface. Mint received 100 of 100 ICMP replies with 7.64 ms
median and 149 ms p95 RTT. Ubuntu received 97 of 100 with 2.47 ms median and
174 ms p95 RTT. The p95 of the absolute RTT difference between consecutive
successful sequence pairs was 144.24 ms from Mint and 169.61 ms from Ubuntu.
The first private SSH goodput transfer timed out before producing a rate.
[Route and numeric ICMP samples](mint-ubuntu-route-diagnostic.json) ·
[Probe source and raw result](mint-ubuntu-route-diagnostic-raw.zip)

The throughput retry used IPv4 loopback forwarding and confirmed equal
payload byte counts at both ends. It measured 93.80 Mbit/s from Mint to
Ubuntu and 93.33 Mbit/s from Ubuntu to Mint, each over about eight seconds.
These are receiver payload rates through the private SSH forward, including
endpoint and cipher costs. The selected route and interface were unchanged
before and after the retry.
[SSH payload rates](mint-ubuntu-ssh-goodput.json) ·
[Probe and raw transfer records](mint-ubuntu-ssh-goodput-raw.zip)

A later read-only device check found the Ubuntu Ethernet port
`enp2s0` down with carrier 0. Its active `wlp3s0` interface had carrier 1.
The dual-machine runs used this Wi-Fi route.

## Wayland viewer KMS completion in a guest

An isolated KVM guest ran production `jwm --backend wayland-udev` on a
1024×768 virtio-gpu output. VirGL used the development machine's Intel
GPU. Two actual remote helpers streamed an owned Xvfb desktop over
authenticated TCP. With the compositor effects disabled, the harness
verified red and blue images through output screencopy, matched Wayland
presentation timestamps to exact JWM DRM submission records, and checked
host acknowledgements and clean shutdown. Guest kernel DRM traces
independently recorded the corresponding completions.

The earlier default effects run discarded the first frame and sent no drawn
ACK. After the fix, three runs with the generated default configuration
matched red and blue output pixels to the compositor's verified interior,
an exact DRM submission and its kernel completion. One run positioned the
guest cursor over the verified interior. The three effects runs returned nine
drawn ACKs and closed cleanly. A fresh direct-render regression returned four
drawn ACKs. [Effects runs](wayland-kms-effects-smoke.json)

The virtual output provides kernel completion evidence. Physical scanout
and input-to-display timing remain unmeasured.
[Earlier guest observations](wayland-kms-guest-smoke.json) ·
[Acceptance script](../../../scripts/test-remote-wayland-kms.py)

### Two active KMS outputs

Four guest boots used two adjacent 1024×768 virtio-gpu outputs at scale 1.
Two used the default effects configuration and two disabled effects. In each
run, the red frame appeared on `Virtual-1`. The test moved and refocused the
same viewer through JWM IPC and changed the source to blue during the move.
The compositor deferred the current commit while the window crossed the seam.
The blue frame and its completion appeared on `Virtual-2`. Three pixel samples
on the other output stayed at the desktop background in both phases.

All eight sampled Wayland completions matched their JWM submission records
and a kernel event on the corresponding CRTC. The four sessions returned
13 drawn ACKs and closed cleanly. These runs cover compositor readback and
guest DRM completion for the tested layout. The initial mixed-scale probe
applied scale 2 to `Virtual-2` while leaving `Virtual-1` at scale 1. It recorded
a discarded first frame and zero drawn ACKs.
[Per-trial pixels and DRM evidence](wayland-kms-multi-output-smoke.json)

### Mixed-scale KMS outputs

Six subsequent guest runs moved the same viewer between adjacent 1024×768
outputs. They covered 1×→2×, 2×→1× and 1×→1.5× with both default effects and
effects disabled. Each phase sampled four quadrants of the source pattern
and checked that the peer output still showed its background. The test also
read the integer buffer scale of the commit corresponding to each Wayland
completion. The 1.5× and 2× outputs received 2× viewer buffers.

All 12 sampled completions matched a guest kernel DRM event on the intended
CRTC. After each completion, the test also read the guest's active VirGL
primary framebuffers. The samples from both framebuffers agreed with
screencopy. The six sessions returned 18 drawn ACKs and closed cleanly. Five
repeat checks passed with 1× outputs, including a single-output cursor overlap
case. Physical scanout and input-to-display latency remain unmeasured.
[Mixed-scale buffer, pixel and DRM records](wayland-kms-mixed-scale-smoke.json)

### KMS power and connector recovery

Four DPMS runs checked the same native viewer and source across On → Off → On.
New Wayland power controls agreed with the DRM connector state. The host sent
the blue test pattern while DRM was Off. The viewer's commit waited at least
until the wake request, then received a matching flip and displayed the pattern
on the same CRTC. The test read the active primary framebuffer in both phases.
Two runs used default effects and two disabled effects.

In two separate runs, QEMU disconnected `Virtual-2` and later reconnected it.
JWM processed the kernel hotplug events, moved the same viewer to `Virtual-1`,
and accepted a move back after `Virtual-2` returned. Both paths displayed fresh
pixels on the expected primary framebuffer in all three phases. The six
successful runs matched 14 sampled completions to guest kernel DRM events and
returned 28 drawn ACKs.

An earlier direct-render run powered off with a keepalive already queued.
That submission had no matching completion. The viewer reported
`wayland_presentation_discarded` and ended the session. Physical display test
coverage remains separate from these virtio results.
[DPMS, connector, pixel and DRM records](wayland-kms-output-recovery-smoke.json)

The Ubuntu peer's read-only DRM check found one enabled connector, the Intel
`eDP-1` used by its active X11 desktop. The other connectors were disconnected.
This peer has no second connected output for an isolated physical hotplug test.

## Managed remote desktop in native terminals

On both the development machine and `ubuntu@192.168.0.78`, an authenticated
loopback session verified that a managed release cleared a held Shift key on
the host. Source selection rejected stale session and source identifiers,
then switched to the authorized 300×200 region at source epoch 2.

A separate view-only test opened the remote desktop panel from the default
command palette in Forge, Anvil, Ember and Frost. Each run used a private
Xvfb display with Metacity and two authorized source regions. The same native
viewer acknowledged JPEG frames for the initial 640×480 source and then for
the selected 300×200 source. Disconnect closed the viewer and ended the
authenticated host connection while the terminal application stayed open.
[Session and terminal test results](managed-session-smoke.json)

The Aether test opened the Remote Desktop tab in native WebKitGTK on X11.
On X11, the same viewer presented both regions and stayed open while the
Terminal tab was selected. A second connection enabled input control. The
host X server reported Shift held before the window manager closed Aether,
then confirmed the key was released and both processes exited normally.
The Wayland view-only run used a separate Weston 13.0.0 headless compositor
with Pixman. The same helper acknowledged both authorized regions and exited
on disconnect. These tests cover top-level native viewers on isolated displays.
[Aether window and viewer results](managed-session-smoke.json)

### Aether embedded X11 video

Aether's Linux X11 window now hosts the Rust viewer in a GTK XEmbed socket.
The WebView carries session controls and measurements. Three native WebKitGTK
runs on private Xvfb displays presented and acknowledged two different shared
regions. The viewer stayed under the same Aether window while switching sources,
resizing the pane from 960×210 to 1180×380, and hiding and restoring it with
the workspace tabs. The runs sampled both colors from the actual screen.
Disconnect removed the socket. Closing Aether during active control released
Shift on the host and both processes exited successfully.

A separate authenticated LAN run streamed live pixels from a private Xvfb
host on `ubuntu@192.168.0.78` into the same native pane. The host recorded
the draw acknowledgment and exited after disconnect. The separate Wayland
viewer passed the Weston source-switch and disconnect regression.
[Embedded video evidence](aether-x11-embed-smoke.json)

### Managed X11 window recovery

The managed helper now accepts an explicit retry budget for X11 view-only
sessions with clipboard off. Forge, Anvil, Ember, Frost and Aether expose
the opt-in through their connection panels. The four standalone adapters
passed their offline Rust build checks. Aether passed four focused Rust
tests, 165 frontend tests and the native application build.

Five managed cases used owned Xvfb displays and an authenticated loopback
fault proxy. Both the top-level viewer and the XEmbed child kept their XIDs
and 930×530 dimensions after recovery. The backoff snapshot cleared the
catalog and timing state. The replacement session published a new catalog
after presentation. Its helper rejected a selection from the old session
and accepted one from the new session. An explicit disconnect interrupted a
stalled greeting, stdin EOF cancelled backoff, and destroying the XEmbed
parent closed a held authentication socket. The default zero-retry case
stopped after one connection. The four earlier direct recovery cases and
all 74 CLI tests also passed.
[Managed helper and adapter checks](managed-x11-window-recovery.json)

The actual Aether WebKitGTK panel launched the pinned helper with three
attempts. The native pane showed the waiting state with the source catalog
and timing display cleared. A fresh authenticated session drew into the same
child and container. On the following transport interruption, closing Aether
while the next greeting was withheld closed the owned socket in 85.92 ms.
The helper, app, host and private displays all exited normally.
[Aether native recovery](aether-x11-managed-reconnect.json) ·
[UI observations, screenshots and replay](aether-x11-managed-reconnect-raw.zip)

### Managed X11 clipboard

Five owned X11 trials ran the managed CLI over authenticated loopback.
Permission stayed off when either side withheld consent or their requested
directions had no intersection. For each allowed direction, progress
appeared while a 5 MiB file was in flight. The receiving clipboard
advertised a staged file with the expected SHA-256. While a separate 8 MiB
file was in flight, Ctrl+Super+Alt+Pause narrowed sharing to text and
images. Both peers confirmed the restriction and removed the partial file.
The narrower policy persisted through source selection. All endpoint
processes exited normally and the closed snapshots cleared clipboard state.
[Managed clipboard results](managed-x11-clipboard-smoke.json) ·
[Owned X11 fixture](../../../scripts/test-remote-managed-clipboard.py)

Forge, Anvil, Ember and Frost each opened their native remote panel and
explicitly requested text from the host. Forge, Ember and Frost received
the exact source text. Ember and Frost also verified reverse copies stayed
local. Anvil showed clipboard off with host consent withheld. Each panel's
disconnect ended the viewer and host session normally.
[Four native panel trials](managed-x11-terminal-adapters.json)

The native Aether XEmbed trial enabled text from the host in the WebKitGTK
settings. With host consent withheld, Aether displayed clipboard off and
kept the viewer's previous selection. With host consent enabled, Aether
displayed the negotiated receive-only direction. A fresh host selection
pasted byte for byte and a copy in the opposite direction stayed local.
Both sessions cleared their policy on disconnect and both private X11
displays shut down normally.
[Aether clipboard and embedded viewer](aether-x11-managed-clipboard-final.json) ·
[Visible status, endpoint snapshots and replay](aether-x11-managed-clipboard-final-raw.zip)

### Managed native Wayland control

The development machine and `ubuntu@192.168.0.78` each ran four integration
tests with private JWM/Smithay compositors, software GLES and authenticated
loopback TCP. A controller command, pointer leave, keyboard leave and window
deactivation each released a Shift key held on the host. The controller
remained in `releasing` while the host compositor was withheld. It reported
`released` after native teardown, and the same viewer surface presented at
320×240 after starting at 400×300. Each run used a single authorization and
closed the viewer and host connection normally. Forge, Anvil, Ember, Frost
and Aether built with the updated controller and passed their existing X11
source-switch and disconnect smoke tests. Aether also passed its Wayland
view-only source-switch smoke test.

The resize tests withheld feedback for the old viewport and then delivered it
late. Before admission, that feedback produced zero grant attempts. During an
active lease, two local resizes kept the same surface, lease and deadline. The
host received the release for Shift held before resizing. A click split across
the second configure produced no native pointer action. After the 480×360
viewport obtained a fresh proof, a click at viewer position (160, 120) reached
the 200×150 source at (66, 50). Both resize tests also passed in eight repeat
runs on the development machine.

The dedicated native CLI suite passed all five process tests. The managed
test started `jwm-remote session` as a child process and observed a 320×240
presented buffer while the original lease and held Shift remained active. It
sent `release_input` through stdin and read `released` from stdout after the
host released Shift. Both processes exited normally after `disconnect`. All
70 CLI unit tests passed.
The full remote suite for `remote-wayland,backend-wayland-udev` passed 1368
tests with 47 explicit ignores.
After the native backing reuse and timing changes, the remote library
suite with `remote-wayland,backend-wayland-udev,remote-h264` passed
1,396 tests with 64 explicit ignores. The owned Intel hardware regressions
above ran separately.

A separate natural-expiry test used an exact 400×300 source and viewer.
After the selected app received the original key release, it repainted a
two-color pattern. The viewer presented a pixel-exact PNG repair within
the original 30-second lease. The test observed the expected natural
expiry with one authorization attempt and no additional input.

The previous helper build rejected the new input selector before authorizing
control. The viewer reported that the host must be updated. X11 view-only
sessions between the same two builds presented frames and disconnected in
both host/viewer arrangements.
[Environments and assertions](managed-wayland-control-smoke.json)

### Native Wayland host source catalog

The host listed three private native windows. The local operator selected the
red and blue windows before the host announced a listener. A native managed
viewer presented red, blue and red pixels again at source epochs 1, 2 and 3
on one authenticated TCP session. Each transition rebuilt the viewer's xdg
role. Its catalog stayed unavailable until the new role presented, even
after delayed feedback for the old role arrived. The controller rejected the
unshared source ID and the stale catalog epoch.

A second run unmapped and remapped the inactive blue window before selecting
it. Both processes reported failure and the viewer received no presentation
for that target. The capture authority belonged to the original window
lifecycle. Both tests passed in the seven-case standalone CLI suite on the
development machine and `ubuntu@192.168.0.78`. Five additional local runs
passed both source tests. The development remote suite passed 1369 tests
with 49 explicit ignores. All 70 CLI unit tests passed.
[Source switching assertions](native-wayland-source-smoke.json)

### Native Wayland source selection with managed input

The original red window received and held Shift. The viewer selected the blue
window while the test paused the host compositor. The old role remained live
during release. The local controller withdrew its catalog and refused another
selection. After the compositor resumed, the old recipient received its Shift
release and the old role was retired. The blue window presented before the
viewer offered **Request Input**. A stale request was refused. A current explicit
request obtained a fresh lease, and only the blue window received both edges of
the A key. Returning to the red window kept it read-only on the same connection.

The eight-case standalone process suite passed on the development machine and
`ubuntu@192.168.0.78`. Ten consecutive runs on the development machine passed
both the managed release and controlled switch cases. All five terminal adapters
built. Aether's real Weston run exercised view-only switching through the native
viewer. The Tauri IPC and browser suites exercised the scope of input requests.
[Assertions and environments](managed-wayland-source-control-smoke.json)

### Native Wayland gestures and popup runtime

The compositor's explicit v6 gesture lease delivered modifier-click and
left-drag events to the original native application receivers. Focus loss,
local takeover, disconnect and capture layout changes balanced held input.
A 2× buffer with an xdg crop at `(8, 12)` delivered captured point `(15, 25)`
as logical root point `(23, 37)`. Moving the crop ended the held drag and made
its old motion invalid. These tests exercised the native protocol before the
remote helper used the gesture lease.

The compositor probe withheld the isolated popup manager with Ubuntu's system
libwayland 1.20. The Ubuntu test process also ran with a privately extracted
libwayland-server 1.21 and passed 106 compositor regression cases. The
development machine passed the same cases with libwayland 1.22 and ran the
dedicated Qt 6.4.2 Widgets probe, which opened a real menu, verified its captured
pixels, selected an item and observed private-seat retirement.
[Assertions and environments](wayland-gestures-popup-runtime-smoke.json)

### Managed Wayland gestures

Independent host and viewer processes negotiated the gesture selector, held
Shift during a drag and presented at least two more video frames while the
button remained down. The original native recipient received the final mapped
position before the button release. Resizing released the old button while
keeping Shift and the same lease. Switching sources released both inputs on the
old recipient. The new source presented read-only, then accepted an explicit
gesture grant on the same connection. Leaving its image balanced its button.

The ten-case standalone suite passed on the development machine and
`ubuntu@192.168.0.78`. All five terminal adapters built. In a real Aether
WebView, selecting the option passed the gesture flag to the helper. Its test
host declined input without consent. [Assertions and environments](managed-wayland-gestures-smoke.json)

### Managed Wayland on mixed-scale outputs

The source and viewer each ran on a private compositor with two advertised
outputs, at 1× and 2×, with different logical origins. The viewer moved to the
2× output during a drag. It kept its 400×300 logical window, presented from an
800×600 buffer and balanced the old button on the original source. The
compositor's preferred scale then overrode the active output's scale, with
the viewer presenting at 2×, 1× and 2× on the same role.

Switching sources released Shift on the first window. The second source
presented at 240×180 logical pixels and accepted a fresh grant. With both the
source and viewer at 2×, the remote click reached the shared window at
(96, 72) logical pixels. The unshared window covering that desktop position
received zero input and supplied no viewer pixels. The case passed on the
development machine and `ubuntu@192.168.0.78` with software GLES.
[Mixed-scale assertions](managed-wayland-mixed-output-smoke.json)

### Gedit on Ubuntu

Gedit 41.0 received `Ctrl+A`, two GTK Unicode-entry sequences and `Ctrl+S`
through the authenticated managed Wayland session. Software GL readback of
the actual viewer surface showed the selection, edit and GTK3 hamburger menu.
Gedit saved `中文` in the temporary UTF-8 document. Clicking **Preferences**
in the streamed menu opened a real Gedit Preferences window on the host.
Both CLI processes exited successfully after the managed disconnect. The
procedure passed three times at each of the 1× and 2× buffer scales with a
640×480 logical capture size. The native v7 test also verified that a menu
click went to the original client, left the physical pointer state unchanged
and was rejected if the captured target's input region changed before release.
Physical screen timing remains open for this workflow.
[Assertions and environment](managed-wayland-gedit-smoke.json)

```sh
JWM_REMOTE_GEDIT_SCALE=1 scripts/test-remote-wayland-cli.sh --gedit
JWM_REMOTE_GEDIT_SCALE=2 scripts/test-remote-wayland-cli.sh --gedit
```

### Firefox through managed Wayland input

Firefox 152.0.5 ran on the development machine with software WebRender and
one drawing subsurface. On the same authenticated connection, the managed
viewer entered two loopback URLs in the address bar, switched tabs, clicked
page content and scrolled with a vertical wheel. The test server received
both typed URLs and the browser's click, wheel and scroll-position reports.
GL readback of the actual viewer showed both pages, the original tab after
the switch and the lower region after scrolling. The workflow passed three
times. Both CLI processes exited successfully after each disconnect.

The native regression opened a gesture lease with a drawing subsurface whose
input region was explicitly empty. While the root had a held button, the
child gained an input region. The original recipient received the release
when the lease ended. The case passed on the development machine and Ubuntu.
[Assertions and environments](managed-wayland-firefox-smoke.json)

```sh
scripts/test-remote-wayland-cli.sh --firefox
```

A separate X11 calibration ran the repository browser workload in Firefox
and sampled the actual cyan tile and scroll text at both endpoints. An
authenticated JPEG Q85 viewer at 960×640 showed 236 distinct changes in
eight seconds while the source showed 472. The longest internal viewer gap
was 46.31 ms. Both endpoints had zero invalid tile reads. This was an unscored
single-machine calibration.
[Calibration results](firefox-x11-pixel-calibration.json) ·
[Raw pixel observations and fixture](firefox-x11-pixel-calibration-raw.zip)

The first scored Firefox batch stopped before motion in all three attempts.
The pixel traces recorded the first-run data reporting notice shifting the
source content roughly 60 seconds into static collection. The workload tab
stayed selected. The retry fixture disables data submission only in its
temporary profile and rechecks the source layout before the motion click.
It preserves the same HTML and scoring windows.
[Failure diagnosis](firefox-x11-initial-run-diagnosis.json) ·
[Original traces and retry fixture](firefox-x11-initial-run-diagnosis-raw.zip)

All three corrected runs completed with 30 seconds of warmup and an exact
60-second observation interval for each static and motion phase. During
motion, the Firefox source produced 58.98–59.00 distinct changes per second.
The 960×640 fixed JPEG Q85 viewer displayed 29.38–29.40 per second and met
the 27 FPS gate in all three runs. Each retained complete host and viewer
telemetry. The static source stayed unchanged. This was the single-machine
X11 software baseline.
[Scored Firefox results](firefox-x11-scored-baseline.json) ·
[Raw pixels, stage windows and verifier](firefox-x11-scored-baseline-raw.zip)

A separate 960×640 X11 trial used the original offline page, Firefox 152.0.5
with software WebRender, and an authenticated JPEG Q85 viewer. The source
button opened the page dialog three times, and Escape closed it each time.
Both drawables showed all six transitions. The viewer’s first matching
raster observation followed the source by 31.23–93.37 ms for opens and
40.79–63.09 ms for closes. The trial measured local software raster
propagation on private Xvfb displays.
[Modal observations](firefox-x11-modal-diagnostic.json) ·
[Raw pixels and fixture](firefox-x11-modal-diagnostic-raw.zip)

The first 1920×1080 dual-machine trial used Firefox on Mint and an X11
viewer on Ubuntu through private SSH reverse forwarding. Each endpoint
recorded 5,400 valid reads with unchanged tile and scroll text during its
60-second static interval. The host and viewer reported the same
authenticated JPEG session. The host reported a protocol I/O error and
the viewer lost the drawable before observing motion. The first
artifact transfer timed out. A subsequent transfer recovered the original
viewer trace. This attempt has a static observation and no motion score.
[Dual-machine attempt](firefox-x11-dual-first-attempt.json) ·
[Source and viewer raw traces](firefox-x11-dual-first-attempt-raw.zip)

A subsequent dual-machine diagnostic completed both 60-second observation
intervals. The source produced 58.98 distinct motion frames per second and
the Ubuntu viewer showed 28.82. The viewer motion gap p95 was 44.30 ms and
the longest gap was 299.84 ms. All four endpoint and phase telemetry sets
identified the authenticated 1920×1080 JPEG session. The host exited
successfully after the viewer disconnected. The peer's Python control-reader
thread then failed during interpreter shutdown. The run retains both
observations and the shutdown failure.
[Dual-machine motion diagnostic](firefox-x11-dual-motion-diagnostic.json) ·
[Raw observations and shutdown logs](firefox-x11-dual-motion-diagnostic-raw.zip)

The next fixture stopped the control reader at the final protocol event.
All three trials completed both static and motion observation intervals
and shut down normally. The source reached 58.95–59.00 distinct motion
frames per second. The Ubuntu viewer reached 28.72, 26.35 and 28.73 FPS
at 1920×1080 JPEG Q85. Two trials reached 27 FPS. Trial two recorded
three one-second bins without a changed frame and a longest gap of
4.87 seconds. Its gap p95 was 44.38 ms. The retained telemetry does not
identify the cause of the long stall.
[Scored dual-machine trials](firefox-x11-dual-scored.json) ·
[Raw observations and replay fixture](firefox-x11-dual-scored-raw.zip)

Two follow-up diagnostics sampled the owned SSH master socket. In the
first, TCP acknowledged no new bytes for 8.00 seconds while the sampled
send queue held about 231 kB. The congestion window fell to one segment
and the retransmission timeout reached 4.70 seconds. The session ended
before motion collection completed. The second completed at 26.22 FPS
with a 5.76-second gap. Around corresponding frame sequences, the viewer
continued sampling and the host-local records showed credit pressure, a
271,712-byte SSH send queue peak and 52,128 bytes of TCP retransmissions.
Host-local and viewer-local timing records use their respective clocks.
The earlier 4.87-second gap occurred before TCP sampling was added.
[Passive TCP diagnostic](firefox-x11-dual-tcp-diagnostic.json) ·
[Socket observations and endpoint replay](firefox-x11-dual-tcp-diagnostic-raw.zip)

A separate 1920×1080 loopback run used the same pinned Firefox executable,
workload and fixed JPEG Q85 settings on two private Mint Xvfb displays.
Each phase retained exactly 60 seconds after 30 seconds of warmup. The
source reached 59.00 distinct motion updates per second and the viewer
reached 29.48. The viewer's motion gap p95 was 43.74 ms and its longest gap
was 56.35 ms. Both endpoints recorded zero tile or glyph changes in their
scored static regions. The startup and end-of-static full source snapshots
had different background pixels and both are retained. This run measures
local software capacity on authenticated IPv4 loopback.
[1080p loopback control](firefox-x11-local-1080p-diagnostic.json) ·
[Raw samples and offline replay](firefox-x11-local-1080p-diagnostic-raw.zip)

Another diagnostic ran the same 1080p Firefox workload on Mint and the
Ubuntu viewer through private SSH. Host NVENC and viewer NVDEC both used
HEVC 4:4:4 throughout the measured session. The source reached 58.95 FPS.
The viewer reached 28.73 FPS, with a 43.89 ms motion gap p95 and a 378.10 ms
longest gap. The source glyph signature stayed fixed during the static
interval, while the viewer recorded 140 changes across three signatures.
The source's exact signature appeared in 254 of 5,400 viewer reads, in 28
bursts whose starts were a median 2.05 seconds apart. The video telemetry
has no per-frame link between those bursts and an encoded access unit. The
run passed the cadence gate and missed the static-image gate.
[Firefox HEVC diagnostic](firefox-x11-dual-hevc444-diagnostic.json) ·
[Raw capture, codec evidence and replay](firefox-x11-dual-hevc444-diagnostic-raw.zip)

A repeat used private instrumented builds with the same 1080p workload and
fixed HEVC 4:4:4 path. The endpoints joined the host's flushed access units
to the viewer's drained X11 copies by authenticated session, source and
geometry epochs, and frame sequence. The session trace contained 5,384
hardware frames and 90 IDRs. The viewer reached 28.92 FPS during motion.
Its static text mask changed 140 times and matched the stable source in
256 of 5,400 scored reads. Fourteen of the 28 entries into the exact mask
aligned with an IDR. Eleven of the 28 exits aligned with a non-IDR picture.
The other entries and exits could not be assigned to one frame using the
copy and raster intervals. The static-image gate remained unmet. Timing
ends at the drained X11 copy.
[Per-frame Firefox diagnostic](firefox-x11-hevc444-idr-diagnostic.json) ·
[Trace observations and offline replay](firefox-x11-hevc444-idr-diagnostic-raw.zip)

A follow-up kept the same scene, 1920×1080 dimensions, NVIDIA hardware
route and Ubuntu viewer binary. The private Mint encoder used NVENC
lossless tuning with constant QP 0. All 5,400 scored static viewer reads
matched the source's thresholded glyph mask. The viewer displayed 28.87
distinct FPS during motion, with a 44.52 ms gap p95 and a 285.10 ms longest
gap. The earlier CBR trace recorded 28.92 FPS, 43.95 ms and 249.06 ms. The
lossless run's video payload averaged 0.906 Mbit/s in the selected static
telemetry windows and 1.502 Mbit/s during motion. The corresponding CBR
rates were 1.112 and 1.410 Mbit/s. The test used one serial run per setting
and measured the selected text region on X11 drawables.
[Lossless Firefox diagnostic](firefox-x11-hevc444-lossless-session.json) ·
[Raw codec, raster and replay evidence](firefox-x11-hevc444-lossless-session-raw.zip)

The host now exposes the lossless encoder setting as an opt-in. A focused
NVENC test decoded all three YUV444 pictures and compared every sample with
the encoder input, including two forced IDRs. In a native-size 1080p Xvfb
session, a deterministic high-entropy picture exceeded the 1 MiB
access-unit limit after initial HEVC playback. Both endpoints recorded ACKs
for later JPEG frames within the same authenticated session, source and geometry.
The viewer also showed the next expected color. A second run masked CUDA
devices only for the host process and negotiated JPEG from startup. Both
runs retained drawn ACKs and closed cleanly.
[Opt-in validation and fallback results](hevc444-lossless-optin-fallback.json) ·
[Typed endpoint observations and replay](hevc444-lossless-optin-fallback-raw.zip)

A matched Firefox pair used the same Mint host binary and the same Ubuntu
NVDEC viewer for CBR and opt-in lossless HEVC 4:4:4. Each static and motion
phase warmed up for 30 seconds and then scored 60 seconds at 1920×1080. Both
arms used the real hardware codec path and exited normally.

| Retained observation | CBR | Lossless constant QP 0 |
| --- | ---: | ---: |
| Static viewer glyph samples matching the source | 251 / 5,400 | 5,400 / 5,400 |
| Visible unique motion FPS | 28.82 | 28.77 |
| Visible motion gap p95 | 43.60 ms | 44.10 ms |
| Longest visible motion gap | 321.84 ms | 299.86 ms |
| Static video payload | 1.137 Mbit/s | 0.873 Mbit/s |
| Motion video payload | 1.395 Mbit/s | 1.494 Mbit/s |

Eight ICMP probes before and after the pair averaged 21.57 and 2.20 ms
respectively, with all replies received. The two arms establish the codec
and static-mask result in this workload. Lossless remains an opt-in.
[Pinned Firefox pair](firefox-x11-hevc444-lossless-option-pair.json) ·
[Endpoint telemetry, raster observations and replay](firefox-x11-hevc444-lossless-option-pair-raw.zip)

### Blender viewport through managed Wayland input

Blender 4.5.16 LTS used a private factory profile with three-button mouse
emulation and continuous grab disabled. The standalone viewer sent an
Alt-left drag, a vertical wheel notch, `Z` to open Blender's in-window
shading menu and `4` to select Wireframe. Blender reported its orientation,
view distance and shading mode through a read-only timer. Actual viewer GL
readback showed the rotated scene while the button was still held, the
zoomed scene, the open menu and the selected shading change.

With continuous grab enabled, Blender requested a Wayland pointer lock and
the session returned to viewing. With continuous grab disabled, Blender
requested whole-window confinement during the drag. The native regression
verified that the compositor deferred that confinement while preserving the
original input recipient. Locks and regional constraints still ended the
lease. [Assertions and environments](managed-wayland-blender-smoke.json)

```sh
scripts/test-remote-wayland-cli.sh --blender
```

A separate Workbench motion calibration played a versioned cube scene from
a fixed orthographic camera. Blender 4.5.16 LTS drew the actual viewport with
llvmpipe in an owned Xvfb display. The authenticated fixed JPEG Q85 viewer
preserved the 960×640 window. Over eight seconds the source showed 351
distinct mesh positions and the viewer showed 218. Both had zero invalid
reads. This was a single-machine software calibration of the raster observer.
[Calibration results](blender-workbench-motion-calibration.json) ·
[Scene, drawn pixels and telemetry](blender-workbench-motion-calibration-raw.zip)

The three scored Workbench runs replayed the same scene with 30 seconds
of warmup and 60 seconds of observation per phase. The source produced
45.17–45.42 distinct mesh positions per second during motion. The viewer
produced 26.93, 27.03 and 27.13 per second. Two runs met the 27 FPS gate.
The longest visible motion gap was 179.88 ms. Static phases recorded no
mesh changes. The verifier reproduced the pixel counts and retained
telemetry windows and confirmed the scene and release hashes. This was a
single-machine 960×640 software Workbench baseline.
[Scored Workbench results](x11-blender-workbench-baseline.json) ·
[Raw pixels, scene and verifier](x11-blender-workbench-raw.zip)

A fixed and adaptive H.264 NVENC preflight used the same Blender scene,
960×640 X11 viewer raster, and private IPv4 loopback connection. The
fixture applied the same area-proportional RGB conversion delay in both
sessions. The adaptive host reduced its target to 24 FPS and its encoded
raster to 720×480 under that pressure. The fixed host kept 960×640.

During the ten-second pressure observation, fixed and adaptive mode
showed 24.70 and 22.50 distinct mesh positions per second, respectively.
The drawable gap p95 measured 66.09 and 46.37 ms. In a stationary Blender
UI crop, the delivered horizontal edge contrast was 73% and 29% of the
source. The polls that first detected a sustained match to the stopped
pose and full-size raster started at 3.38 and 23.45 seconds. The timing
uses the local viewer drawable and reported encoded geometry.

These two preflight sessions qualify the fixture. The scored comparison
uses repeated ABBA trials with 60-second windows for static, ordinary
motion and motion under pressure.
[Preflight metrics](x11-blender-nvenc-adaptive-pilot.json) ·
[Drawable observations, PNG pairs and replay](x11-blender-nvenc-adaptive-pilot-raw.zip)

Two scored ABBA blocks completed all eight trials on the same 960×640 scene
and pinned NVENC build. Each phase retained 60 seconds. Ordinary motion
reached 27 distinct visible positions per second in zero of four fixed trials
and one of four adaptive trials.

| Median across the four trials per mode | Fixed | Adaptive with resolution changes |
| --- | ---: | ---: |
| Ordinary-motion visible FPS | 26.91 | 26.85 |
| Ordinary-motion drawable gap p95 | 66.02 ms | 66.15 ms |
| Pressured-motion visible FPS | 24.67 | 22.10 |
| Pressured-motion drawable gap p95 | 56.94 ms | 46.99 ms |
| Pressured-motion longest drawable gap | 156.82 ms | 167.21 ms |
| Pressured-motion interface edge contrast retained | 72.61% | 28.81% |
| Stop to sustained full-resolution clarity, observed upper bound | 4.57 s | 24.49 s |

Adaptive pressure p95 improved in two pairs and increased in two. The adaptive
arm encoded 720×480 under pressure and had lower visible cadence and lower
interface edge contrast in every pair. The source interface crop was identical
between arms. The test used one machine, private Xvfb, and injected RGB
conversion pressure. The resolution policy remains opt-in.
[Scored comparison](x11-blender-nvenc-adaptive-abba.json) ·
[Endpoint timing, raster observations and replay](x11-blender-nvenc-adaptive-abba-raw.zip)

A separate matched pilot kept the encoded raster at 960×640 in both modes
and allowed the adaptive arm to change its frame rate. After 45 seconds of
pressure warmup, the 30-second sample retained 72.6% of interface edge contrast
in both arms. Fixed and adaptive observed 24.43 and 21.77 distinct positions
per second. Their drawable gap p95 values were 46.29 and 56.54 ms. The stop to
clarity observations were both about 4.47 seconds. The adaptive trial showed
slower visible updates while retaining the original raster.
[Rate-only pilot results](x11-blender-nvenc-rate-only-pilot.json) ·
[Rate-only drawable and telemetry archive](x11-blender-nvenc-rate-only-pilot-raw.zip)

A separate fixed-rate comparison tested the X11 capture wait with the
same 960×640 Workbench scene, 30 FPS request and NVENC codec. The candidate
uses a sub-millisecond `ppoll` wait and preserves the deadline across
interruptions. The regression test checked readiness with a zero timeout
and a positive wait on a real socket. Five pacing tests also passed.

Two ABBA blocks each used a 30-second warmup and a 60-second score per
trial. All eight trials qualified. Every candidate trial reached 27
visible FPS, and the measured interface edge contrast matched in every
trial.

| Measure | Millisecond wait | Precise wait |
| --- | ---: | ---: |
| Median host scheduling rate | 29.26 FPS | 29.76 FPS |
| Median visible rate | 26.93 FPS | 27.42 FPS |
| Median of trial p95 visible gaps | 65.01 ms | 52.22 ms |
| Longest visible gap across trials | 166.87 ms | 178.33 ms |

The candidate reduced the visible-gap p95 in three of four matched pairs.
The longest gap increased in three pairs by 2.52, 12.00 and 33.26 ms. The
longest-gap result kept the candidate out of the default capture path. The
archive contains the candidate patch, both binary hashes, the build options,
the matched measurements and the replay.
[Fixed-rate capture cadence comparison](x11-blender-fixed-capture-cadence-abba.json) ·
[Patch, endpoint observations and replay](x11-blender-fixed-capture-cadence-abba-raw.zip)

Isaac Sim acceptance was skipped at the user's request on November 21,
2026. The earlier Isaac Sim 4.5 report records local EGL startup and UI
checks. The Mint and Ubuntu benchmark hosts had no executable or matching
Docker image in the inspected locations.
[Historical local EGL check](../../x11-egl-gles.md)

## Aether terminal semantics

The real PTY bridge suite ran on the development machine and
`ubuntu@192.168.0.78`. The test dropped a delta, obtained a snapshot on the
same shell and retried a lost recovery response. The old attachment could no
longer ACK or submit input. The resumed shell returned a variable set before
the interruption. On Ubuntu that 16×80 publication encoded to 25,613 bytes
as a row delta and 101,391 bytes as a full snapshot.

A native WebKitGTK run detached an open shell, then reattached its read-only
view. The terminal exposed the original marker and output generated by the
shell while detached. Each marker appeared once in the copied grid.

The standalone SSH relay also passed five runs from the development machine
to `ubuntu@192.168.0.78`. Each run used four independent OpenSSH connections
against one private PTY owner. The client abandoned an unacknowledged delta,
cut SSH and released a shell command that wrote output while no terminal
relay was connected. Recovery returned the same shell and generation on an
unacknowledged retry. The stale generation could no longer ACK, write, or
close the session. The current attachment recovered the disconnected output
and read a shell variable set before the cut. A fresh read-only attachment
refused input. Explicit close reaped the remote PTY.

The 16×80 initial snapshot record was 101,431 bytes and the dropped delta
record was 12,968–12,969 bytes, including their SSH relay JSON envelopes. The
cross-machine test reconstructed the terminal grid through the standalone
relay.

Aether's native Terminal tab passed three repeatable WebKitGTK runs from the
development machine to the same Ubuntu host. The test killed Aether's owned
OpenSSH child and released a shell command while the terminal had no relay.
**Recover output snapshot** created a fresh OpenSSH child and displayed the
original marker and the disconnected output. After recovering the snapshot,
new input read a variable from the original PTY. Explicit detach and
**Reconnect view** used a third SSH child and restored a read-only grid.
An attempted write from that view produced no file on the host. **Close shell**
reaped the remote PTY, and the native application exited successfully.
[Terminal stream results](terminal-semantic-smoke.json)

## Plan acceptance status

| Plan check | Measured result and scope |
| --- | --- |
| Continuous motion at a 30 FPS request | At 1080p, the revised XTerm scroll-copy detector reached 27 FPS in three matched pairs, and both Firefox HEVC arms reached 27 FPS in one matched pair. At 1440p, all eight aligned native trials reached 27 FPS inside a 4K root. At exact 4K, all four native X11 preallocation trials reached 27 FPS. In the separate Workbench comparison, the precise wait reached 27 FPS four times and increased the longest gap in three matched pairs. [XTerm](#visible-terminal-scrolling) · [Firefox](#firefox-through-managed-wayland-input) · [1440p](x11-native-1440p-viewport-prefetch-target-aligned-abba.json) · [4K](#native-x11-presentation) · [Workbench](x11-blender-fixed-capture-cadence-abba.json) |
| Software input release | The source-click dual-machine batch verified all 300 scored releases. Its three p95 upper bounds were 4.21, 4.57 and 4.49 ms. The separate SSH observer return path contributes to the measurement. Three localhost sets had p95 of 2.46–2.55 ms. [Input results](#clipboard-input-during-an-upload) · [Qualified dual-machine batch](x11-release-all-software-latency-source-click-setup.json) |
| Sustained resource use | The one-hour native 4K retry held 28.57 visible FPS. Host and viewer anonymous RSS leveled off at +480 KiB and +132 KiB, respectively. The separate Ubuntu FFmpeg NVENC run held the same anonymous RSS from minute 17.5 through minute 60. [One-hour native 4K soak](x11-native-4k-prefetch-resource-hour-soak.json) · [One-hour encoder test](ffmpeg-nvenc-only-hour-soak.json) · [Incomplete first 4K attempt](x11-native-4k-prefetch-resource-hour-soak-incomplete.json) |
| Adaptive hardware video | Opt-in X11 H.264 NVENC reduced dimensions and FPS, recovered both, and passed viewport, input, source and JPEG fallback checks in owned sessions. In eight matched Blender trials, stressed p95 changes varied by pair. The adaptive trials had consistently lower pressured-motion cadence and interface contrast. [Resolution results](x11-nvenc-adaptive-resolution.json) · [Scored Blender comparison](x11-blender-nvenc-adaptive-abba.json) |
| Source change, resize and recovery | Owned X11 and Wayland runs checked pixels and input recipients across scope changes. The managed X11 fixture retained top-level and embedded windows, refreshed the session catalog and cancelled a stalled handshake. Native Aether repeated the embedded workflow. KMS power and connector changes passed on two guest virtio outputs. Physical hotplug and GPU device loss await hardware runs. [Direct recovery](#follow-up-checks) · [Managed recovery](#managed-x11-window-recovery) · [KMS](#kms-power-and-connector-recovery) |
| Terminal integration | Forge, Anvil, Ember, Frost and Aether opened real native sessions. The Aether PTY relay recovered the same shell after the SSH connection was cut. [Adapters](#managed-remote-desktop-in-native-terminals) · [PTY recovery](#aether-terminal-semantics) |
| Application scenes | The 1080p HEVC lossless arms met the cadence and selected glyph-mask checks for XTerm and Firefox. XTerm also retained exact glyph matches in all 900 post-scroll reads. Blender Workbench has independent motion results. Isaac Sim acceptance was skipped at the user's request. [XTerm](x11-terminal-scroll-hevc444-lossless-option-pair.json) · [Firefox](#firefox-through-managed-wayland-input) · [3D workloads](#blender-viewport-through-managed-wayland-input) |
| 1080p wired physical interaction, p95 ≤ 50 ms | Unmeasured. The tested cross-machine route used Ubuntu Wi-Fi. A physical trigger and a high-speed camera or calibrated optical apparatus are needed for the three sets of at least 100 samples. [Route](#mint-and-ubuntu-network-route) · [Physical measurement protocol](../../remote-workload-benchmark.md#physical-latency-and-acceptance) |
