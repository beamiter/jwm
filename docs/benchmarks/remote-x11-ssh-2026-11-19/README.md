# X11 remote control over SSH, 2026-11-19

This initial benchmark used two physical machines and the installed
`jwm-remote` helpers with transport v3 and application protocol v35. The
viewer was an i7-13650HX laptop with an RTX 5060 Laptop GPU on Wi-Fi. The host
was an i9-14900K desktop with an RTX 4090 D. [summary.json](summary.json)
records the analysis checkout, the installed helpers' SHA-256 hashes, the
drivers and the display modes.

| Workload | Retained time | Sent FPS | Video payload |
| --- | ---: | ---: | ---: |
| Live DP-0 desktop, 1393×783 encoded | 20.10 s | 9.35 | 1.10 Mb/s |
| Isolated X11 static image, 1280×720 encoded | 15.04 s | 0.20 | 0.78 kb/s |
| Isolated X11 moving bands, 1280×720, initial | 15.01 s | 11.13 | 8.79 Mb/s |
| Isolated X11 moving bands, 1280×720, repeat | 15.04 s | 23.33 | 19.88 Mb/s |

The live view-only session submitted its first frame about **0.84 s** after
launch, including SSH startup. It captured the 2560×1440 DP-0 monitor through
the NVIDIA-compatible Composite overlay path with core GetImage and CPU
scaling. Capture including scaling averaged **44.9 ms**, and capture start
through drawn acknowledgement averaged **53.6 ms**. The host suppressed 238
unchanged frames in the retained live windows. The live sent FPS reflects
natural desktop activity.

The controlled runs used private Xvfb displays on both machines, a
2560×1440 source, a 1280×720 viewer, CPU scaling, the default 30 FPS ceiling
and adaptive JPEG. The moving-band generator targeted 30 FPS.
Clipboard was off throughout. Each controlled workload retained three
five-second host windows after discarding its first window. The second run
kept the same settings. Ambient load and the Wi-Fi link could vary.

In the initial motion run the host sent 21.2, 1.0 and 11.2 FPS across the
three retained five-second windows. Between the first two, mean
send-to-drawn-ACK time increased from **19.8 ms** to **303.9 ms**, while mean
capture time changed from 27.5 ms to 29.3 ms. The repeat stayed between
23.0 and 24.0 FPS. A separate 40-packet ICMP sample had a median RTT of
**3.5 ms**, a P95 of **111 ms** and zero losses. The ACK metric covers the
application feedback path, including viewer work.

| Controlled response | Samples | Median | P95 |
| --- | ---: | ---: | ---: |
| Injected key to matching X11 viewer pixel, initial | 36 | 59.6 ms | 175.9 ms |
| Injected key to matching X11 viewer pixel, repeat | 36 | 67.2 ms | 95.7 ms |
| Pointer warp across the image edge to released status, repeat | 20 | 2.3 ms | 2.4 ms |

Each input run had four additional warmup samples. The viewer observed all
72 measured input markers. Latency ends at software observation of the X11
drawable, before physical scanout. Edge release used a roughly 2 ms status
poll, which limits the precision of the handoff numbers.

[samples.json](samples.json) contains the host telemetry windows, input
samples including warmups, edge release observations and numeric RTT
samples. [summary.json](summary.json) contains the aggregates. Video
payload rates exclude transport overhead. The host stage P95 values in the
summary are the largest reported recent P95 per five-second window.
