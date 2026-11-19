# X11 remote desktop comparison with Sunlogin, 2026-11-19

Both products connected to the same physical X11 host and shared its
2560×1440 DP-0 output. The synthetic window drew scrolling bands at a
target of 30 source FPS. The viewer probes counted distinct source frame
markers in the X11 drawable. Each motion trial retained 15 seconds after
about five seconds of warmup. Input timing began at local XTEST F8
injection and ended when the expected response pixel appeared in the X11
drawable. Each complete input trial measured 36 responses after four
warmups.

| Trial | Projected image | Visible FPS | Input median | Input P95 |
| --- | ---: | ---: | ---: | ---: |
| JWM SSH, display on | 1393×783 | 7.19 | 263 ms | 383 ms |
| JWM SSH, DPMS off observed at end | 1393×783 | 0.80 | 272 ms | 387 ms |
| Sunlogin 16.6.0.32198, UI reported P2P | 1398×787 | 15.06 | 125 ms | 169 ms |

All three complete input trials observed 36 of 36 responses within the
2.2-second timeout. Sunlogin and the JWM power-state trials ran about
two and a half hours apart. The applications used their available session
settings, and the projected image dimensions describe the local window's
image area. The active Sunlogin capture, codec, internal resolution and
hardware path were uninstrumented. Its [performance mode documentation](https://service.oray.com/question/49261.html)
describes configurable quality and frame-rate behavior.

## JWM with the display awake

The host used `overlay/cpu-resize/core-getimage/damage/cursor-events`.
On proprietary NVIDIA X11, automatic scaling chooses synchronous core
GetImage at the full source dimensions, followed by conversion and CPU
downscaling. This compatibility path addresses earlier unfinished
Composite frames with XRender. The measured capture interval includes
the complete call. Individual time spent in X server or GPU synchronization,
pixel transfer, conversion and resizing requires separate counters.
[Capture implementation](../../../src/remote/x11_capture.rs#L1588) ·
[NVIDIA compatibility rationale](../../remote-control.md#half-drawn-frames-on-nvidia)

Three complete host motion windows logged 118 captures and 118 sent frames
in 15.18 seconds. The weighted mean capture time was **125.1 ms**, with
116.4–137.1 ms across window means. A serial capture loop at 125.1 ms per
capture can deliver roughly eight captures per second, close to the
**7.19 FPS** observed by the independent viewer probe. JPEG window means
were 33.9–43.4 ms and credit wait window means were 2.1–3.3 ms.
Send-to-drawn-ACK averaged 56.6 ms, including client work, and capture
start through drawn ACK averaged 221.8 ms. These stage timings overlap
and cover different samples. The capture timing and capture count explain
most of the observed video throughput limit in this run.

X11 key injection has its own connection, and the visible response still
waits for a subsequent captured frame. The 263 ms measured key response
covers input transport, receipt and update, arrival of a useful capture,
encoding, transport and viewer rendering. The capture-to-ACK measurement
covers a different endpoint and starts later in the pipeline. A timed host
input receipt and an exact frame marker would be needed for a per-key
stage breakdown.

### NVIDIA BGRX capture fix

The NVIDIA overlay reported 11-bit color intensity precision and the
standard 8-bit RGB masks for a depth-24 image stored as 32-bit little-endian
pixels. The intensity precision field had kept the overlay on the generic
per-pixel decoder. The fix selects the packed BGRX decoder from the visual
class, channel masks and image format. The regression test compares both
decoders byte for byte.

A follow-up held DP-0 awake and used the same moving bands, capture mode and
SSH viewer build for both host binaries. The source was 2560×1440 and the
encoded image was 1392×783. Each build retained three telemetry windows
after warmup, totaling 15.22 seconds before and 15.09 seconds after the fix.

| Host or viewer metric | Before | After |
| --- | ---: | ---: |
| Mean pixel conversion | 98.80 ms | 5.77 ms |
| Mean full capture | 126.54 ms | 39.30 ms |
| Mean capture start to drawn ACK | 188.11 ms | 87.27 ms |
| Viewer drawn ACKs per second | 7.75 | 20.15 |

The viewer acknowledged 118 drawings before the fix and 304 afterward.
These figures use JWM telemetry. The earlier visible-FPS and input-to-pixel
rows use the independent X11 viewer probe.

### JPEG tile encoder follow-up

The candidate uses a faster baseline JPEG encoder for tile atlases. Both
encoders use 4:4:4 sampling, and both builds use the same viewer and the DP-0
fixture above. The fixed-quality comparison held quality at 85 and ran the
baseline, candidate, candidate and baseline in order. Each trial retained
three full five-second windows after warmup.

| Fixed quality 85 | Before | After |
| --- | ---: | ---: |
| Mean JPEG encode per atlas | 34.18 ms | 21.48 ms |
| Mean capture start to drawn ACK | 101.68 ms | 95.70 ms |
| Viewer drawn ACKs per second | 18.39 | 21.50 |
| JWM payload rate | 14.21 Mbit/s | 19.72 Mbit/s |

The totals include a candidate window where send-to-drawn-ACK averaged
1.31 seconds. The two baseline trials also had different payload sizes.
In a separate deterministic 60-frame replay, encoded bytes increased from
432,832 to 433,299 and aggregate decoded PSNR changed from 45.217 to
45.227 dB.

With the default adaptive mode, three trials per build measured mean JPEG
encoding of 37.74 and 22.27 ms, respectively. Viewer drawing fell from 18.88
to 14.44 ACKs per second, and mean capture-to-drawn-ACK rose from 97.60 to
115.60 ms. During the reverse-order adaptive repeat, ICMP probes on the same
path saw round trips up to 5.09 seconds and 2.7% loss. The slow JPEG windows
and the network stalls are both represented in the aggregates. A separate
candidate trial capped at 20 FPS drew at 19.11 FPS over the same path, with
83.76 ms mean capture-to-drawn-ACK. Its unrestricted baseline drew at
20.67 FPS and averaged 84.88 ms.

### Hardware H.264 over SSH

An opt-in H.264 build used NVENC on the RTX 4090 D host and Intel VA-API on
the same viewer. The comparison used the BGRX-optimized JPEG baseline,
the same viewer build, the same DP-0 moving bands and the same 1392×783
encoded extent. Two trials per build ran in baseline, H.264, H.264,
baseline order and retained three full telemetry windows each.

| Metric | JPEG baseline | Hardware H.264 |
| --- | ---: | ---: |
| Mean host encode | 38.54 ms | 10.62 ms |
| Viewer drawn ACKs per second | 18.47 | 21.63 |
| JWM payload rate | 17.10 Mbit/s | 4.35 Mbit/s |
| Mean capture start to drawn ACK | 107.70 ms | 77.37 ms |

The two H.264 runs drew 27.28 and 15.98 FPS. The latter included a window
with four drawn ACKs, and the concurrent ICMP probe measured a 5.07-second
maximum round trip during the comparison. Both H.264 trials used 4:2:0
chroma at a 6 Mbit/s encoder target. The reported payload rate uses actual
wall-clock throughput. These are software-observed drawing measurements.

### HEVC 4:4:4 clarity

An isolated X11 session displayed identical synthetic terminal text and
one-pixel color lines at 1392×784. The older host selected H.264 4:2:0 with
a 6 Mbit/s target. The newer host selected HEVC 4:4:4 with a 10 Mbit/s
target. Chroma RMSE on 91,017 selected colored pixels in the drawn viewer
images was 48.33 for H.264 and 32.93 for HEVC. A separate 30-frame NVENC
test at the same 6 Mbit/s target measured 48.08 and 23.49 on its text and
line mask. Both tests used synthetic content at a fixed resolution.

## JWM with display power saving

Three host motion windows had **385 scheduled capture attempts, 15 captured
frames and 10 sent frames**. The session still reported
`overlay/cpu-resize/core-getimage/damage/cursor-events`, peak in-flight
frames was one, and sender credit waits were below 0.001 ms in those
windows. The loss of frame cadence occurred before the encoder. This
matches reduced useful updates from the overlay or its XDamage gate while
the display was in power saving. The run sampled DPMS off at the end, and
the awake rerun confirmed DPMS on at both ends. Distinguishing compositor
redraw cadence from XDamage notification and gating behavior requires
separate counters. [XDamage decision path](../../../src/remote/x11_capture.rs#L1331)

Earlier JWM motion runs with the same fixture delivered 12.54 and 10.20
visible FPS. One of those runs included a five-second window without a
visible frame and a host-observed send-to-ACK spike. Wi-Fi and transport
stalling remain relevant to the tails. The awake rerun's frame-count and
stage data point first to host capture cost, and the sleeping rerun adds
an upstream capture-cadence problem.

[samples.json](samples.json) preserves viewer samples, typed host
telemetry and the partial earlier input attempts. [summary.json](summary.json)
records the results and window selections.
