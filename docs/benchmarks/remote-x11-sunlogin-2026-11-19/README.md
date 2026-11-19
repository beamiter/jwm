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
