# Changelog

Notable user-visible changes to JWM are recorded here. Components in this
monorepo use independent Semantic Versions.

## [Unreleased]

### Added

- `jwm-remote` provides an authenticated JWM-to-JWM remote desktop MVP for
  x11rb/xcb X11 sessions. It captures the shared Composite overlay out of
  process, sends bounded JPEG frames, maps a native X11 viewer back into host
  coordinates, and uses XTEST for explicitly enabled input. The direct LAN
  mode is authenticated but deliberately documented as unencrypted/trusted-LAN
  only; loopback plus SSH is the confidential deployment path.
- `behavior.recording_max_height` caps the encoded height, scaling the capture
  down to fit and preserving aspect ratio. Every downstream cost scales with the
  pixel count, so capping a 4K display to 1080p cuts the readback, the pipe and
  the encoder to a quarter, and the downscale itself is free because the capture
  blit already resamples the region into the output. 0, the default, records at
  the captured resolution.
- Status bars show the focused window's desktop icon beside its title. JWM
  publishes the window's application identity in shared-memory protocol v14 and
  `xbar_core` resolves it through the freedesktop desktop-entry and icon-theme
  lookup; `visibility.client_icon` and `ModelConfig::resolve_client_icons` turn
  it off.
- The bar's layout menu offers every layout the running window manager has
  rather than a fixed three. Protocol v14 carries the layout count and the
  layout in use, so the menu also marks the active entry, drops entries a
  compositor cannot enter, and keeps a newer compositor's extra layouts
  reachable.
- Tag-driven release automation with quality gates, an installable bundle, a
  Git source archive, SHA-256 checksums, and artifact provenance.
- Tested versioned install, upgrade, rollback, and uninstall operations.
- Compatibility, upgrade, and release-process documentation.

### Changed

- The shared-memory protocol is v14. JWM and every bar must be rebuilt and
  restarted together, which the existing layout/version validation enforces.
- `display::CANONICAL_LAYOUTS` is now the single source for JWM's layout ids,
  names, symbols, labels and cycle order; `LayoutEnum` derives from it.
- No stable release has been published. The root `0.2.0` manifest version
  remains a development version, not a support commitment.
- CI now treats Clippy correctness, suspicious, and performance diagnostics as
  errors and explicitly tests the Linux action and D-Bus provider adapters.

### Fixed

- Screen recording no longer freezes the desktop. The compositor used to write
  each captured frame — 8 MB at 1080p — straight into ffmpeg's 64 KiB stdin pipe
  from its render loop, so whenever the encoder fell behind, the one thread that
  serves input and repaints for every client parked inside `write`. Frames now
  go to a writer thread through a short bounded queue and are dropped when the
  encoder cannot keep up. Stopping a recording no longer waits for ffmpeg to
  rewrite the file for `+faststart` either.
- An active recording no longer pins the compositor into a continuous
  full-screen redraw. It now composites only on the frames it actually captures,
  and the X11 and Wayland event loops sleep until the next one is due instead of
  polling at 1 ms. The capture clock advances by a whole frame interval, so a
  30 fps recording samples at 30 fps rather than drifting toward 20.
- `get_recording_status` now reports what the recording is actually achieving,
  not just what it was configured for: frames captured, frames dropped because
  the encoder could not keep up, elapsed time, and the effective capture rate.
  A recorder silently running at a third of the requested rate used to look
  identical to a healthy one until the file was played back.
- Screen recording converts to NV12 on the GPU instead of shipping RGBA to the
  encoder, on both the X11 and the Wayland backend. A fullscreen pass packs the
  composited frame into a target laid out as NV12, so the readback, the copy out of mapped memory, the pipe and ffmpeg's
  read all carry 1.5 bytes per pixel instead of 4 — exactly 62.5% less, at every
  resolution — and the encoder needs no conversion pass at all. Measured over
  twenty seconds of continuously changing 1080p content, the encoder process
  fell from 301 to 84 CPU ticks. The vertical flip moved into the same pass, so
  `-vf vflip` is gone too. Drivers that cannot hold the packed target fall back
  to the previous RGBA capture.
- The mouse cursor is drawn into recordings on the GPU rather than blended into
  every frame on the CPU. The X11 recorder re-uploads the cursor image only when
  its shape changes rather than once per frame; the Wayland recorder, which has
  no cursor image to sample because KMS scans the real pointer out on its own
  plane, draws the same synthesised arrow it always has.
- Recordings are no longer colour-shifted in most players. Frames were converted
  with BT.601 and the file was tagged with nothing, so ffmpeg-based playback
  guessed BT.601 and looked correct while mpv, VLC and browsers applied the
  usual "HD means BT.709" rule and showed pure red as (255,23,0). The GPU
  conversion uses BT.709 limited range — verified bit-exact against ffmpeg's own
  conversion across primaries, black, white and grey — and the stream is now
  tagged to match.
- Hardware video encoding actually works now. `behavior.recording_encoder`
  defaults to `auto`, but the probe that was meant to detect NVENC asked it to
  encode a 64x64 frame — below NVENC's minimum frame size — so it failed on
  every machine that had a working NVENC and `auto` silently fell back to
  libx264. The VAAPI probe failed for its own reason: it fed a software frame to
  an encoder that needs a hardware one. Both probes now use a 256x256 frame and
  build the same hardware frame the real command does. On an NVENC machine this
  cut the encoder process's CPU by 80-92% (1483 -> 301 ticks over twenty seconds
  of continuously changing 1080p content, 733 -> 82 for an ordinary desktop).
- `-pix_fmt yuv420p` is no longer forced on the hardware encoders. They convert
  from RGB on the GPU, so naming an output format only inserted a CPU
  conversion pass in front of them; removing it measured 17% off NVENC's process
  CPU with byte-identical colour. The software encoder still pins yuv420p,
  without which libx264 negotiates the far more expensive yuv444p.
- The recording capture target is now 8-bit RGBA rather than the 10-bit format
  the blur pipeline uses. Frames are read back as 8-bit bytes and encoded to an
  8-bit stream, so the extra precision was being paid for and discarded, and a
  format-matched readback stays on the driver's fast path.
- Screen recording no longer recomposites and re-encodes a screen that has not
  changed. It captures when a client draws, when an animation runs, or when the
  cursor moves, and otherwise keeps the encoded timeline alive with a 2 fps
  heartbeat instead of 30 full-screen captures a second. On a 1080p30 recording
  this cut the compositor's CPU by 82% with a moving pointer and 90% on a still
  desktop, for a file with the same frame count, duration and contents.
- The pipe carrying frames to the encoder is widened from the default 64 KiB to
  1 MiB, which turns a 1080p frame from 127 blocking writes into 8 and leaves
  the encoder more slack before the recorder has to drop a frame.
- Capturing a recording frame no longer makes a synchronous X server round-trip
  for the cursor. `XFixesGetCursorImage` — the only source for both the cursor
  image and its true root position, since motion over a client window never
  reaches the window manager — now runs on a sampler thread with a connection of
  its own, and the capture path takes the latest sample without waiting. An
  unchanged cursor shape reuses its pixel buffer instead of reallocating per
  frame.
- Screen recording competes far less with the desktop for CPU: the software
  encoder runs at `veryfast` with half the cores rather than `medium` with all
  of them, and ffmpeg is started one nice level down. Its per-frame progress
  line no longer accumulates in `/tmp`.
- Starting a recording no longer re-probes the available hardware encoders and
  the ALSA demuxer on every keypress, and querying recording status no longer
  runs `ffprobe` on the window manager's thread once the output has been
  validated.
- Bar tag glyphs no longer depend on which font fontconfig happens to hand a
  private-use code point: an installed Nerd Font is named explicitly in the
  font description (configurable as `presentation.icon_font`). The gear and
  home tags previously resolved to Arial, which draws unrelated shapes there.
- The default Tao/pixels bar now activates a control only after a matching
  press and release on the same node, and follows JWM's authoritative bar
  height instead of leaving a four-pixel layout gap.
- Installed payload ownership and modes are normalized instead of preserving
  untrusted extraction metadata; path traversal, symlink, and special-file
  payloads remain rejected.
- Production X11 session entries no longer force the optional WaterLily layer
  onto shared `/tmp` test endpoints.

## Versioning note

The root `jwm`, `jwm-bridge`, `jwm-portal`, `shared_structures`, `xbar_core`,
provider crates, and each bar are separate SemVer components. A JWM bundle
records the exact set it contains; its tag does not replace component versions.

[Unreleased]: https://github.com/beamiter/jwm/commits/master
