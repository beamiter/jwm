# Remote GPU pipeline: first-stage readiness and copy audit

## Scope and current result

This change supplies a read-only inventory tool and opt-in synthetic encoder
probe. It does **not** add a remote video codec, export a screen, enable DMA-BUF
capture, or establish zero-copy. Existing source authorization, input leases,
codecs and presentation proofs are unchanged. The audit below was made against
candidate `2a18e757` and rechecked against master `85dae205` before integration.
The remote experiment branch is not merged by this change. Recheck these
boundaries when changing the capture or codec implementation.

## Actual remote data path

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

## Next implementable milestones

### A. Same authorized source, GPU capture buffer

Extend the existing **same-source** image-copy client, not a fallback to a wider
output capture. Negotiate device/format/modifier, import a bounded pool, attach
it to the source session, and explicitly synchronize capture completion. Keep
FD ownership, plane offsets/strides, dimensions, source/geometry epoch and
release fence in a typed GPU-frame record. Reject unknown modifiers, invalid
planes and stale epochs. A view-only GPU frame must never satisfy an existing
CPU-based input/presentation proof accidentally. If the proof cannot be
preserved, keep this prototype view-only and explicitly report it.

Do not reuse DMA-BUF storage until both capture and encode consumers are done.
On revoke, source disappearance or resize, invalidate pending frames and safely
retire them. If import fails, fall back only within the original source grant;
no screenshot fallback with broader visibility.

### B. Hardware encode from that buffer

First prove one device/format pair with a synthetic GPU surface (known colors,
odd dimensions, strides, fences). Then prove real authorized capture import.
NVENC may need EGL/CUDA interop; VAAPI DRM PRIME import needs matching device,
format/modifier and sync handling. A subprocess `hwupload` from CPU pixels is
an explicitly labeled fallback, not the GPU-native success criterion.
Measure capture/readback bytes, CPU conversion time, import stalls, encode time,
payload bytes, frame age and buffer occupancy. Preserve color range/matrix and
alpha handling. Fail closed on stale capture; do not attach live user pixels to
public diagnostics.

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
