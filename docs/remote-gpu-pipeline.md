# Remote GPU pipeline: first-stage readiness and copy audit

## Native X11 viewer path

On a verified Intel X11 combination, H.264 VA-API frames can now reach the
viewer window through EGL and DRI3. Use the X11 viewer's render node with
`jwm-remote doctor --probe-gpu-present /dev/dri/renderD128`, then opt into
`JWM_REMOTE_GPU_PRESENT=/dev/dri/renderD128` for the actual session. See the
[configuration and fallback behavior](remote-control.md#experimental-scheduling-controls).

The VA-API surface remains leased until the GLES fence completes. GLES
converts the NV12 planes to a separate XRGB8888 buffer. The X11 connection
owns its imported pixmap and frees it after replacing the retained frame.
The viewer's existing X11 completion event still gates acknowledgement.
The host path in this validation captured an Xvfb source in CPU memory and
encoded H.264 with NVENC. Wayland presentation and hardware scanout
timing have separate acceptance gates.
[Native X11 test evidence](benchmarks/remote-upgrade-2026-11-20/README.md#native-x11-presentation)

The same-device Intel Xwayland path also presents exact 3840×2160
frames with zero CPU downloads in the verified runs. The optional
`JWM_REMOTE_X11_PREFETCH_TARGET=1` worker allocates one fresh XRGB8888
target ahead. Rendering consumes the target once, and the X11 connection
then owns the imported pixmap through its normal retirement. Native X11
telemetry distinguishes allocation on the viewer thread from allocation
on the worker.
[Intel 4K measurements and lifecycle proof](benchmarks/remote-upgrade-2026-11-20/README.md#native-x11-presentation)

The optional `doctor --probe-gpu-encode /dev/dri/renderD128` test uses
libva PRIME_2 to retain the NV12 plane offsets, pitches and modifier when
importing into a second VA-API frames context. The imported surface
retains the exported frame through encoder completion. The probe
encodes a synthetic picture with `h264_vaapi` and checks the IDR's four
colors after decoding.

## Native X11 host path

The X11 host can opt into same-device H.264 encoding with
`JWM_REMOTE_GPU_CAPTURE=/dev/dri/renderD128`. It copies the authorized
rectangle to a fresh server pixmap, exports the XRGB8888 DMA-BUF through DRI3
and checks the X11 fence before passing the exported descriptor to the video
worker. The bounded mailbox owns the descriptor until replacement or dequeue.
The video worker imports it into VA-API with its exact layout and modifier.
VA-API processes the source directly into an NV12 encoder surface, with
BT.709 limited-range signaling. Logical encoded dimensions are rounded
down to even values within the agreed viewport.

The probe must pass on the actual X11 device. Driver rejection at startup
retains ordinary negotiation. A live failure retires the GPU path and takes
a fresh JPEG capture inside the original grant. The source and geometry
barriers still gate publication and the first hardware picture after a scope
change carries an IDR. Host telemetry records the actual capture path and
DMA-BUF import, conversion, submit and output stages.
[Host configuration](remote-control.md#experimental-scheduling-controls)

## Native Wayland host path

A fixed read-only Wayland window can use linear XRGB8888 DMA-BUF capture
and H.264 encoding on the compositor's DRM device. The host waits for
capture completion and owns a fresh buffer until VA-API finishes the GPU
conversion to NV12. The import uses aligned storage and processes only
the visible source rectangle. A failed capture import or render triggers a
fresh SHM capture of the same authorized window. The native Wayland viewer
can hardware decode H.264 and use SHM or opt-in DMA-BUF presentation.
[Host configuration](remote-control.md#experimental-scheduling-controls) ·
[Native Wayland capture test evidence](benchmarks/remote-upgrade-2026-11-20/README.md#native-wayland-host-capture-and-encode)

## Native Wayland viewer path

The view-only H.264 viewer can use its compositor's DRM render node via
`JWM_REMOTE_GPU_PRESENT`. With matching `linux-dmabuf` feedback, a GLES pass
samples VA-API NV12 and writes an immutable XRGB8888 DMA-BUF for the current
window size and scale. The viewer keeps up to two imported buffers in flight.
Buffer release retires storage and presentation feedback gates the ACK. A
recoverable import failure downloads the decoded picture and presents it
through SHM in the same window and session.

Run `scripts/test-remote-wayland-cli.sh --gpu-viewer /dev/dri/renderD128`
with a hardware VA-API driver and FFmpeg development files to exercise the
actual CLI against two private compositors. The test joins host GPU capture
and encode with viewer DMA-BUF presentation through resize and source
switching. It exercises viewer import rejection and host DMA-BUF withdrawal
independently. The managed CLI exposes `hardware_download_ms` alongside its
presentation metrics. Physical scanout requires separate acceptance evidence.
[Native Wayland viewer test evidence](benchmarks/remote-upgrade-2026-11-20/README.md#native-wayland-viewer-presentation)

## Scope and current result

This is the historical audit for `2a18e757` and `85dae205`. For current
capabilities, including explicit hardware video and the local codec probes,
see [capture and viewer capabilities](remote-control.md#capture-and-viewer-capabilities).

This change supplies a read-only inventory tool and opt-in synthetic encoder
probe. It does **not** add a remote video codec, export a screen, enable DMA-BUF
capture, or establish zero-copy. Existing source authorization, input leases,
codecs and presentation proofs are unchanged. The audit below was made against
candidate `2a18e757` and rechecked against master `85dae205` before integration.
The remote experiment branch is not merged by this change. Recheck these
boundaries when changing the capture or codec implementation.

## Remote data path in the historical audit

| Boundary | Source evidence | Consequence |
|---|---|---|
| Compositor capture render | `src/backend/udev_kms.rs`: `render_into_client_dmabuf`, SHM `map_texture` branches | Server supports a GPU render target for eligible client DMA-BUFs; this is not proof the remote client selects it. |
| Remote capture buffer | `src/remote/wayland_capture.rs`: `create_buffer`, `wl_shm::Format::Xrgb8888`; `DmabufDevice/Format` events ignored | Current remote client chooses SHM, not exported GPU surfaces. |
| Capture read/convert | Same file: `read_exact_at`, `decode_xrgb`, optional `imageops::resize` | Reads XRGB into CPU memory, converts to RGB, and may CPU-resize. |
| Compression | `src/remote/frame.rs`, `tile.rs`, `png_atlas.rs` | Software JPEG and exact PNG tiles consume CPU images. No H.264/HEVC/AV1 remote negotiation. |
| Decode and presentation | `frame.rs`; `wayland_viewer.rs`: `draw_recyclable`, `upload_xrgb`; `wayland_viewer_damage.rs` | Decoded RGB is converted/written into bounded XRGB SHM slots. Damage reduces writes, not a GPU-native decode path. |
| Separate screen recording | `src/backend/compositor_common/media.rs`: `select_recording_encoder` | NVENC/VAAPI select ffmpeg encoders fed CPU raw video. This is not remote GPU transport. |

Do not label SHM file writes as GL texture uploads. The viewer compositor may
upload/import storage later; physical copies and driver behavior require tracing.
For a full frame, the capture XRGB read alone handles `width * height * 4`
bytes (8,294,400 at 1920x1080), and the RGB output is `*3` bytes (6,220,800).
These are logical buffer sizes, not measured bus traffic or total allocation.
Damage, scaling, reuse, driver upload and encoding all change actual cost.

## Readiness tool

```
python3 scripts/remote-gpu-readiness.py
python3 scripts/remote-gpu-readiness.py --probe-encode
python3 scripts/remote-gpu-readiness.py --probe-encode --render-node /dev/dri/renderD128
python3 scripts/test-remote-gpu-readiness.py
```

Default mode lists ffmpeg build support and render nodes only. Opt-in probes
encode three generated black 128x128 frames for each advertised hardware codec;
they do not capture desktop, microphone, network, or write media files. Processes
are bounded to ten seconds each and retained output to 1 MiB per stream; output
size is polled, so a short burst may exceed the disk threshold before termination.
All descendant processes in the probe group are cleaned up. On many-GPU hosts,
select one `--render-node` to bound total time. Exit zero means the report was
produced, **not** hardware readiness; inspect each status. Failed inventory is
unknown availability, never absence of support. Diagnostics may contain driver
information; review reports before sharing.

Evidence levels remain separate:

1. `advertised`: codec compiled into installed ffmpeg.
2. Probe `passed`: three CPU-generated frames encoded in this process/device.
3. `dmabuf_import_verified`: always false in this tool.
4. `remote_codec_negotiation_verified`: always false in this tool.
5. `end_to_end_zero_copy_verified`: always false in this tool.

In this cloud environment ffmpeg advertised all six NVENC/VAAPI codecs. All
NVENC probes failed because `libcuda.so.1` was absent; no DRM render nodes were
visible, so VAAPI probes were not attempted. This is not evidence about the
user's RTX 5080. No device installation or driver-setting change was attempted.

## Remaining native Wayland milestones

### A. Capture reuse and further formats

Validate additional device, format and modifier combinations against the
same-source capture contract. For a bounded capture pool, keep FD, layout,
source and geometry epoch with each record until both capture and encode are
complete. Reject unknown modifiers, invalid planes and stale epochs. Preserve
the existing input proofs before extending the GPU path to scoped control.

Do not reuse DMA-BUF storage until both capture and encode consumers are done.
On revoke, source disappearance or resize, invalidate pending frames and safely
retire them. If import fails, fall back only within the original source grant;
no screenshot fallback with broader visibility.

### B. Wider hardware encode validation

Extend the authorized-window test across drivers, odd dimensions and varying
strides. Verify matching device, format, modifier and fences for each import
path. Measure capture/readback bytes, CPU conversion time, import stalls,
encode time, payload bytes, frame age and buffer occupancy. Preserve color
range, matrix and alpha handling, and fail closed on stale capture.

### C. Negotiated stream and hardware viewer

A temporal codec cannot be substituted for a JPEG enum without protocol work.
Negotiate codec/profile/level, resolution, color metadata, keyframe recovery,
queue limits and decoder reset. Keep reliable control/lease release independent
of stale-video dropping. Treat static exact repair coordinates/generation and
video reference frames explicitly; an old exact patch must not overwrite a newer
video state. Presentation feedback, decoder completion and input-to-photon are
different evidence. DMA-BUF decode-to-display requires its own lifecycle tests.

### Release gates

Use synthetic tests in cloud, then authorized RTX5080 plus a second GPU vendor.
Exercise unsupported import, driver reset, capture revoke, resize, rapid source
switch, mixed monitor color/refresh, backpressure and disconnect. Measure actual
input-to-photon externally under identical setup against the chosen baseline.
No production switch based only on advertised codecs or synthetic encode success.
