# Opt-in static text fidelity

The integrated JPEG/PNG path is available on master at `eb45d52e`. The earlier
standalone baseline remains separate evidence, not a production throughput claim.

## Using the read-only feature

Both peers must run **application protocol v25**; older application versions are
rejected rather than silently downgraded. Authenticated transport remains v3.

Enable the host permission and request it explicitly in a read-only viewer:

```sh
jwm-remote host --allow-static-lossless
jwm-remote connect HOST --view-only --static-lossless
```

Use the normal connection/authentication options for your deployment. For an
explicitly selected Wayland source, the host example becomes:

```sh
jwm-remote host --wayland-select --once --allow-static-lossless
jwm-remote connect HOST --viewer-backend wayland --view-only --static-lossless
```

Both flags default off. This is a codec permission, not an input permission.
The first release is read-only: keyboard, pointer/control, one-shot text,
clipboard, input grabs and remote source-resize requests cannot be combined
with it. The authorization cannot be saved into a connection profile. Existing
read-only source selection and lifecycle restrictions still apply; this option
never selects another source or grants access to previously hidden surfaces.

Initial frames and moving content continue using JPEG. Each tile has its own
two-second **exact** observed RGB stability gate. On captures without ordinary
JPEG dirty work, bounded round-robin PNG batches repair eligible stable tiles;
a changing neighbor does not reset the stable text tile's clock. Ordinary dirty
work always takes JPEG priority, so continuously changing captures can still
delay repairs. The stability gate is paid per changed tile, not per batch. The scheduling
candidate adds 250 ms of quiet after any exact raw change and separates successful
batches by at least 250 ms from the preceding record's completed flush. Capture/FPS
and transport credit still apply; a delayed writer never causes catch-up bursts.
Before writing a PNG, the candidate compares any newer queued capture's full
source/geometry/dimensions/RGB and discards the repair if content was superseded.
Identical newer captures do not cancel it. A discarded repair consumes no credit
and does not clear exactness debt. This is best effort before writing, not
preemption of an already-sent record. The former policy used a 100 ms batch gap;
[real-pipeline comparison](remote-lossless-scheduling-ab.md) is still pending. The global quiet guard
intentionally gives changing content priority: updates more frequent than every
250 ms can indefinitely postpone repair of otherwise stable neighboring tiles.
It is not guaranteed per-tile convergence during continuous animation. A failed PNG
encode instead backs off for two seconds without clearing exactness debt.
Any subsequent raw change, including a single channel
changing by one, restarts that tile's stability and makes its pixels owe an exact
repair again. Backpressure and batch limits can delay completion beyond two
seconds; this is not a two-second convergence guarantee. Only successful
record writes and flushes commit exactness.

“Lossless” refers to the RGB image at the negotiated **encoded dimensions**.
It cannot recover detail lost during scaling, guarantee physical display color
accuracy, or establish application/input latency. PNG compression has CPU and
byte costs; the noise negative control below is deliberately included because
lossless is not uniformly cheaper than JPEG.

## Why this experiment

`TileEncoder::has_quality_debt` compares previously sent JPEG quality with the
current target. Once those match, an unchanged frame can remain suppressed even
if its displayed RGB differs from the capture RGB. JPEG quality 100 is not a
lossless guarantee. The existing host refinement cadence is two seconds, with
bounded tile selection, but it only repairs this quality-level debt.

The proposed target is **post-scaling encoded RGB**, not the original application
buffer or pre-scaling source. An eventual lossless repair must not promise to
recover pixels discarded by scaling.

## Reproducible bounded baseline

Run the ignored test
`offline_static_text_production_jpeg_and_separate_png_baseline` explicitly, with
`--nocapture --test-threads=1`. It opens no display, socket or GPU. Use the existing
locked dependencies and record the revision, dirty-tree status and build profile
alongside its JSON. Debug timings are descriptive, not release throughput.

Three deterministic 320×180 inputs are generated: handwritten 5×7 bitmap letters
plus one-pixel colored lines on light and dark backgrounds, and a high-entropy
negative control. They are synthetic stimuli, not screenshots, fonts, CJK
legibility coverage or a representative application corpus. Each input hash and
exact SSE/channel-count/MSE/max-error measurement is retained.

The JPEG arm calls production `TileEncoder` and `TileDecoder` at qualities
75/90/100, then confirms ordinary identical-frame suppression and cleared
quality-level debt. The comparison encodes the same full RGB image using the
existing PNG 0.18 dependency and checks exact decoded equality. It is **not** a
PNG tile atlas or a negotiated production decoder. Its byte total excludes
future tile framing; comparing it with JPEG tile payload is exploratory, not a
transport-savings claim.

There are two warmup and three measured trials per point, 60 rows total. Each
compressed output is capped at 1 MiB; each decoded image is exactly 172,800 bytes.
Inputs are generated sequentially, and the report stores metrics, not pixel
history. A CI timeout should bound the complete invocation independently of
these byte/count limits. Raw operation times are reported separately; JPEG
materialization/measurement is outside decode time, whereas PNG's decode path
includes output allocation. Do not turn their ratio into an optimization claim.

## Minimum integration contract for a later decision

These are proposed integration requirements, not properties established by the
standalone PNG comparison above.

### Narrow negotiated record and decoder bounds

- Keep lossless repair explicitly negotiated and default off. PNG is a
  **delta-only repair**, not an alternative initial/source-reset keyframe.
  Unknown or unnegotiated records fail before decoded-atlas allocation.
- Bound the **actual atlas**, including unused packing cells, to 65,536 pixels
  (196,608 RGB bytes), not merely the sum of selected tile areas. Validate
  dimensions, tile coordinates/counts, checked arithmetic and aggregate scratch
  budgets before allocation or decompression.
- Cap compressed PNG data at 1 MiB and set decoder-internal allocation limits.
  A byte cap alone does not bound inflated output or decoder working memory.
- Accept only the deliberately narrow RGB8, non-interlaced PNG subset. Require
  valid signature, exactly one valid IHDR, correctly ordered contiguous IDAT
  chunks and a terminal IEND; check chunk lengths and CRCs. Reject APNG,
  ancillary metadata, unknown chunks, duplicate/out-of-order structural chunks,
  unsupported layouts and bytes after IEND. The current helper does **not**
  canonicalize the zlib stream inside IDAT: png 0.18 can tolerate extra compressed
  streams or decompressed rows there. Such surplus remains subject to the
  1 MiB compressed cap and the 4 MiB decoder-internal allocation limit; the
  helper returns only exactly the declared, bounded RGB output. Do not describe
  this as rejecting every form of internal compressed-stream surplus.
- Test malformed/truncated envelopes, CRC failures, unsupported formats,
  oversized atlases, excessive compressed data, decompression limits and
  trailing data. A decoder error must leave the displayed reference unchanged.

### Exactness is separate transactional state

- Track independent exact-repair debt against **current post-scaling RGB**.
  JPEG quality 100 never discharges it. Even a one-channel change of ±1 that
  ordinary video's tolerance suppresses invalidates the affected tile's exact
  state; detecting only ordinary dirty tiles is insufficient.
- Select bounded batches of stable current tiles; dynamic video remains JPEG.
  A prepared PNG is only a candidate. Mark tiles exact only after the complete
  corresponding record is successfully written and flushed, and only for the
  input revision that was encoded. Partial writes, discarded work, changes
  while pending or failed flushes must not clear debt or advance the reference.
- Initially clear exactness on scroll-copy and on source/geometry resets.
  Copying an already lossy displayed canvas does not become exact merely
  because the raw overlap matches. A later propagation optimization would
  need its own decoded-reference proof and tests.
- Never treat an old cached PNG as a new native input proof. A mandatory proof
  still needs a fresh captured native serial, new wire sequence and the exact
  actual-presented ACK mapping. Reusing compression work must not invent a
  capture, satisfy a newer revision's debt or bypass any permission/freshness
  gate.
- Measure time and bytes until exact RGB convergence, not just one compression
  call. Include noisy content, changing inputs, backpressure, partial failure,
  resize and malicious compressed envelopes. Preserve the unchanged defaults if
  the additional resource cost is unsuitable.

Any later acceptance must include real authenticated transport and presented-frame
checks in owned CI environments. This offline experiment establishes neither
application compatibility nor physical input-to-visible latency.

## Initial offline run (2026-11-10)

The exact test passed once in the cloud with the debug profile: 60 rows including
warmups, 3.51 seconds total test duration. All 172,800 RGB channels were compared
per image. Raw JSON, log, compiler/CPU metadata, dirty-tree status and source
hashes are preserved in
[`benchmarks/static-lossless-preparation-2026-11-10/report.json`](benchmarks/static-lossless-preparation-2026-11-10/report.json)
and its adjacent `metadata.json`. These record the original dirty-tree test
candidate, not released-code performance acceptance or the later integrated PNG
atlas implementation.

| Synthetic input | JPEG quality | Tile bytes | SSE | Separate PNG bytes / SSE |
| --- | ---: | ---: | ---: | ---: |
| Light glyphs/lines | 75 | 38,377 | 13,738,025 | 1,927 / 0 |
| Light glyphs/lines | 90 | 53,604 | 3,283,189 | 1,927 / 0 |
| Light glyphs/lines | 100 | 130,802 | 120,665 | 1,927 / 0 |
| Dark glyphs/lines | 75 | 37,858 | 16,428,650 | 1,931 / 0 |
| Dark glyphs/lines | 90 | 52,802 | 3,165,866 | 1,931 / 0 |
| Dark glyphs/lines | 100 | 120,134 | 211,158 | 1,931 / 0 |
| Noise negative control | 75 | 75,357 | 66,978,604 | 173,073 / 0 |
| Noise negative control | 90 | 111,807 | 11,345,377 | 173,073 / 0 |
| Noise negative control | 100 | 243,342 | 227,331 | 173,073 / 0 |

Across the three measured trials per point, JPEG encode ranged from 16.95 to
24.47 ms for the glyph inputs, and decode from 23.75 to 40.39 ms. Standalone PNG
encode ranged from 24.88 to 26.35 ms, and decode from 5.00 to 5.48 ms. For noise,
JPEG encode ranged from 22.73 to 36.56 ms and decode from 40.30 to 47.29 ms; PNG
encode was 53.91–56.09 ms and decode 3.83–4.18 ms. These are debug CPU samples,
with the different timing boundaries stated above, not comparative end-to-end
latency or throughput results.

The run establishes a concrete fidelity gap even at JPEG quality 100 and a
potentially useful text-like lossless repair. It also demonstrates why noisy
content cannot be assumed cheap: the standalone lossless image is larger than
JPEG 75/90 and takes longer to encode in this run. Atlas integration, idle
scheduling, actual resource budgets and transport/presentation remain untested.


## Separate integration candidate

The new candidate uses a negotiated PNG flag only for nonempty ordinary deltas;
initial and geometry/source-reset frames remain JPEG. It is restricted by the
host to explicitly opted-in read-only sessions. The encoder tracks previous
observed RGB only in this mode, so a ±1 channel change resets stability and exact
repair debt even when normal tolerance suppresses transmission. Batches use
single-column padded tile packing bounded to 65,536 pixels, selected round-robin.

Planning/encoding does not clear exact debt. Commit, called only after a complete
successful flush, also compares a digest of the encoded RGB, dimensions, source
dimensions and geometry epoch with the immutable capture supplied by the caller.
A changed capture forces recovery instead of claiming exactness. Failed writes,
discarded encodes and decoder rejection do not establish an exact repair. PNG
repairs are not native input proof records and never change input permissions.

The earlier full-image standalone PNG sizes/timings are not measurements of this
integrated atlas path. Integrated byte totals, convergence and actual presentation
still require separate codec and authenticated transport tests.


## Verified integration boundary (2026-11-10)

Master `eb45d52e` passed all four mainline workflows: [Wayland](https://github.com/beamiter/jwm/actions/runs/42871308519),
[X11](https://github.com/beamiter/jwm/actions/runs/42871308768),
[CI](https://github.com/beamiter/jwm/actions/runs/42871308597) and
[Eva](https://github.com/beamiter/jwm/actions/runs/42871308605). The new path was
verified with owned Wayland servers, authenticated TCP and software GL. Tests
check exact post-encoding RGB, permission refusal, default-off behavior, small
raw changes and a continuously blinking tile beside static content.

The blink fixture uses fixed 500 ms source updates and checks actual client
`take_presented` completion observations against those deadlines. This is not a
hardware presentation clock or a general 500 ms latency guarantee. One candidate
run had only about 0.249 ms of margin; the raw observations are retained rather
than claiming generous performance headroom. Physical GPU/DRM and two-machine
desktop acceptance remain unverified.

The [same-input offline policy comparison](benchmarks/static-lossless-tile-stability-2026-11-10/README.md)
uses simulated convergence time and the same candidate codec in both arms. It
is not a historical-binary or network performance comparison. Combined popup
and passive source-resize behavior requires its own integration evidence;
individual passing fixtures do not establish that combination.

## Selected-window family and passive resizing

The existing read-only options can be combined explicitly:

```sh
jwm-remote host --wayland-select --once --wayland-popups --wayland-resize-follow --allow-static-lossless
jwm-remote connect HOST --viewer-backend wayland --view-only --static-lossless
```

The host follows changes made locally to the selected source; this does not
permit the viewer to resize that window. Viewer `--follow-source-size` is an
active remote-resize request and remains incompatible with static lossless.
Popup capture is restricted to the selected window family, not another top-level
window. Lock and unmap remain terminal; remapping does not create a new grant.

The dedicated combination regression is newly added and awaits its own CI result.
It checks unscaled matched-size pixels, popup creation/movement/clipping/closure,
size changes and a return to the original size with a new geometry epoch. A
bounded test-only barrier suspends an already encoded PNG before the publication
lock so a resize ABA can advance; successful-write evidence must exclude the old
scope and identify a fresh JPEG keyframe before exact repair resumes. This is
not a promise that bytes already sent before revocation disappear from the peer.
