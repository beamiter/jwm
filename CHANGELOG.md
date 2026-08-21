# Changelog

Notable user-visible changes to JWM are recorded here. Components in this
monorepo use independent Semantic Versions.

## [Unreleased]

### Added

- A windowed list in the shell — the launcher's matches, the notification
  history, the Wi-Fi/Bluetooth/clipboard/wallpaper pickers and the Hub itself —
  draws a scroll indicator in the card's right-hand margin. The window manager
  sends the compositor a slice of a longer list, and until now nothing on the
  card said so.
- A hairline separates a shell panel's list from its footer hint, so the line
  naming the keys stops reading as one more row.
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

- The shell panels are mutually exclusive. `Alt+F10` pressed on top of
  `Alt+F9`'s calendar now closes the calendar and opens the Shell Hub in its
  place, instead of the press going nowhere; the same holds for every panel key,
  in both directions, and from the IPC socket. The keyboard and pointer grabs
  and any temporarily leased compositor are handed straight over rather than
  released and retaken, so the swap costs a frame instead of parking every
  hidden window twice and resetting the compositor's runtime state. A panel that
  cannot open — no `nmcli` for the Wi-Fi picker, clipboard history switched off,
  one output for the display layout — leaves the panel you had on screen. Each
  key still toggles its own panel off, and nothing at all replaces the lock
  screen.
- The modal shell card holds a stable width. The launcher re-measures its match
  list on every keystroke, and the card used to resize under the typing; it now
  only ever grows while a panel is open, in fixed steps, and starts over when
  the panel is replaced or closed.
- The shell card's selection highlight springs between rows instead of
  teleporting, and is placed rather than slid on a freshly opened panel so it
  never travels in from a row of the list it replaced.
- Every theme's footer hint is now held to WCAG's 3:1 contrast floor and the
  typed query line to the 4.5:1 body ratio. The default `glass` theme drew its
  hint at 1.6:1 — the least legible text on the panel was the line naming the
  keys — and its `hint_ink` has been darkened accordingly.
- `jwm-remote` viewers report their window size and the host stops encoding
  pixels the window cannot show. A 640-wide viewer previously received a
  1280-wide image and discarded half of it on arrival. `--max-width` becomes a
  ceiling rather than a target: a request may only narrow it, so a peer can
  never make the host spend more readback, encode time or bandwidth than the
  operator allowed. The encode is fitted inside both the viewer's width and its
  height, whichever binds harder: clamping width alone made one `--max-width`
  mean very different amounts of work depending on monitor arrangement, since a
  2560x2880 stacked root became 1280x1440 — 2.7 times the pixels of a
  side-by-side root at the same flag — and a portrait root was barely clamped
  at all. Measured on a 3440x1440 host as a viewer resized from 1280 to 640
  wide: capture 18.3 -> 9.5 ms per frame, encode 2.2 -> 0.6 ms, capture-to-ACK
  4.9 -> 2.4 ms.
- The `jwm-remote` viewer no longer clears its whole window before every frame.
  The backing pixmap is retained between frames and an upload only touches the
  image rectangle, so the bars around it stay correct until the letterbox
  itself moves; the per-frame fill also blocked on a synchronous round trip.
  Viewer draw time fell from 7.8-8.1 ms to 6.0-6.6 ms at native resolution.
- `jwm-remote` captures when the X server reports a change rather than on a
  fixed timer; `--fps` became a rate limiter instead of a schedule. The loop
  slept blindly to its next grid point and never watched the X connection, so
  at the default 12 FPS every interaction waited a mean of 42 ms for a tick
  that had nothing to do with it. The wait is bounded by one frame interval, so
  a missed notification degrades to exactly the old cadence rather than
  stalling, and events x11rb buffered while a capture waited on its own reply
  are drained before the descriptor is polled. Measured on a live 3440x1440
  session, `scheduled` fell from a constant 60 per five-second window to 21-51,
  following real screen activity.
- `jwm-remote` host telemetry leads with the capture paths actually in use —
  `mode overlay/xrender/shm/damage/cursor-events` — and announces every
  transition as it happens. Four separate facilities can degrade themselves at
  runtime, each of them expensive, and each previously announced only once by a
  line that had long since scrolled away.
- `jwm-remote` now encrypts the session, not just authenticates it. Screen and
  input payloads are sealed with ChaCha20-Poly1305 (transport version 2). This
  mattered most for input: a key press is a three-byte record, so a passive
  listener on the LAN could previously reconstruct a typed password, SSH
  passphrase or 2FA code byte for byte with no image analysis. The handshake
  gained magic and a version field, and both proofs and both traffic keys are
  now derived over the complete transcript, so rewriting the version changes
  every derived secret and fails closed instead of negotiating weaker terms.
  Small records are padded to 64/256/704-byte buckets, because length alone
  otherwise distinguishes a keystroke batch from a pointer run from a
  release-all; inter-keystroke timing is still not hidden. There is no forward
  secrecy — a leaked key file decrypts recorded sessions. The 16-byte tag is
  smaller than the 32-byte HMAC it replaces, and each record is now assembled
  in one reusable buffer and written with a single `write_all` instead of three
  unbuffered socket writes. Measured at native 3440x1440, the largest payloads
  this carries, the telemetry `write` stage was unchanged at 0.0-0.1 ms.
  ChaCha20-Poly1305 is pure Rust: no C toolchain or system dependency is added.
- `jwm-remote` video is now dirty-tile delta coded instead of whole-frame JPEG,
  which is application protocol version 4 (update both machines together). Each
  frame ships only the 16-pixel tiles that differ from the pixels the viewer was
  last sent, packed into one atlas image and encoded once. Measured on a
  3440x1440 desktop over loopback: 10.1 -> 0.5-0.9 Mbit/s at the default
  `--max-width 1280`, and 53.3 -> 1.8-2.6 Mbit/s at native resolution. Encode
  time fell from 60 ms to 1.5-2.7 ms per native frame, removing a ceiling that
  had capped the achievable rate near 16 fps regardless of link speed, and
  capture-to-ACK fell from 80-89 ms to 12-14 ms. Comparing against the last
  transmitted pixels rather than the previous capture means the small tolerance
  that absorbs scaler dither cannot accumulate into visible drift. Host
  telemetry gained `keyframes` and `tiles A/B (P% dirty)`.
- `jwm-remote host --once` returns the session's own result, so a scripted run
  that failed is distinguishable from a clean one by exit code.
- `jwm-remote` backpressure refresh is no longer clamped up to a fixed 250 ms
  floor, which had added fifteen frame-times of staleness at `--fps 60`.
- `jwm-remote` now downsizes both Composite-overlay and root-fallback frames
  with XRender before readback, overlaps capture with JPEG/network sending
  through a one-frame latest-wins queue, reports stage latency and dropped
  stale frames, and enforces an absolute negotiation deadline on both peers.
  Root capture first uses `IncludeInferiors` to copy the root and its same-depth
  children into a full-size staging pixmap, then scales into the small readback
  target; it never creates a Render Picture directly from the root window.
  Staging is capped at 64 MiB, resize/topology races retry once, and allocation,
  request or extension failures retain the same-frame full-resolution readback
  plus CPU resize.
- The JWM remote application protocol is now version 3 and deliberately rejects
  version 2, so both endpoints must be upgraded together. Cumulative frame
  acknowledgements still cap video at two frames beyond what the viewer has
  actually drawn, and sustained backpressure throttles redundant X11 captures.
  Input now travels in authenticated batches of at most 128 operations and 641
  bytes. Adjacent pointer positions are latest-wins without crossing key,
  button or release-all edges; the host preflights each complete batch before
  queuing it in order and flushing XTEST once.
- Host and viewer now publish five-second and final pipeline telemetry without
  changing the wire protocol. Host reports remain live through zero-send credit
  or socket stalls and separate capture-mailbox replacement from the viewer's
  decoded-frame replacement. Cumulative ACK output distinguishes the one proven
  `drawn-acks` target from all `retired` credits and inferred
  `viewer-superseded` frames; capture/send-to-ACK timings end when the host
  receives the ACK, while the viewer separately reports decode, queue and draw
  time.
- Host JPEG quality now uses same-setting display ACK feedback without changing
  the wire protocol. The existing `--jpeg-quality` value is its upper bound;
  repeated ACK RTT, viewer-supersede and frame-credit pressure cause a
  multiplicative decrease, while an FPS-scaled healthy run of at least three
  seconds recovers additively by one. The default floor is 40,
  `--jpeg-quality-floor` adjusts it, and `--fixed-jpeg-quality` restores a
  constant quality. In-flight quality epochs prevent late old-setting ACKs from
  causing a second decrease, and payload size remains diagnostic rather than a
  discrete threshold.
- The remote host suppresses an exactly unchanged captured frame before JPEG
  encoding. It compares source and image geometry plus every RGB byte against
  the last fully written wire frame; suppressed samples consume no frame
  sequence, display credit or quality decision. A successful unchanged frame
  is still sent every four seconds, safely inside the viewer's shared video
  idle timeout, and telemetry separates `unchanged-suppressed` from successful
  `unchanged-keepalive` frames.
- Composite-overlay capture now uses XDamage notifications to avoid
  reading back an entirely static desktop on every scheduled tick. Damage,
  compositor-owner, geometry, cursor-shape and pointer-position changes still
  capture immediately, while a two-second forced refresh feeds the existing
  four-second unchanged-frame keepalive. Root mode does not negotiate XDamage,
  and unavailable or rejected Damage requests permanently retain the proven
  per-tick capture path without ending the session. Damage object creation and
  destruction remain checked; per-frame Subtract is queued before the
  synchronous readback ordering barrier, and a server rejection is drained as
  an asynchronous Damage error in the same frame to disable gating. Host
  telemetry reports event-suppressed ticks separately as `damage-skipped`
  rather than conflating them with capture-mailbox backpressure.
- `jwm-remote` uses MIT-SHM 1.2 file-descriptor segments for local X11 image
  readback when the server and transport support them. It reuses a bounded
  mapping and falls back to core `GetImage` on the same drawable and frame if
  shared-memory setup or capture fails.
- Host capture converts the common depth-24, 32-bpp little-endian TrueColor
  readback directly from native BGRX rows into owned RGB frame storage. The
  fast path checks the current image format and visual masks on every eligible
  window readback and respects native row stride; nonstandard visuals and
  formats retain the generic `PixelLayout` decoder.
- Remote X11 capture now caches the root geometry, compositor owner and XFixes
  cursor shape behind checked Core/RandR and XFixes notifications. Stable
  frames reuse cursor pixels and their scaled image while querying only pointer
  position. All source modes observe compositor-owner epochs so a restarted
  compositor receives a fresh capture-inhibitor notification, while Root mode
  never acquires the overlay. Resize and owner races are authoritatively
  reconciled and retried once before disabling XRender or falling back from the
  overlay, and each unavailable notification path independently retains its
  previous polling fallback.
- Host video records now have one absolute 10-second budget across every
  partial socket write and the final flush. A peer can no longer keep a frame
  sender alive indefinitely by slowly draining bytes and restarting the
  per-system-call timeout; any incomplete record fails closed and triggers the
  existing session/input cleanup path.
- The X11 remote viewer reuses its native upload buffer, writes the common
  depth-24/32-bpp little-endian TrueColor layout directly, and uploads that
  layout through a reusable MIT-SHM 1.2 file-descriptor segment when available.
  It waits for the matching completion event before reusing shared pixels and
  retries rejected uploads with core `PutImage` on the same frame. The viewer
  retains a fully presented backing pixmap for Expose events; nonstandard
  visuals keep the generic/core path, and resize bursts allocate only their
  final size.
- Enlarging the remote viewer no longer makes the client resample and upload a
  window-sized image for every frame. When both fitted dimensions upscale the
  encoded image, XRender 0.10+ retains and scales a source-sized server pixmap
  into the letterboxed backing pixmap. This improves large-window presentation
  without increasing the host's encoded resolution or network traffic;
  one-to-one/downscaled presentation and unavailable XRender retain the
  existing CPU upload path.
- The remote viewer no longer wakes every four milliseconds while idle. It
  blocks on the X11 connection, a video-receiver wake descriptor, and the next
  heartbeat, telemetry or deferred-key deadline, while checking x11rb's
  already-buffered event queue before sleeping. With input negotiated, normal
  window close flushes `ReleaseAll` before one authenticated `Close`;
  X11/network failures shut the transport down directly so host cleanup
  releases input. Session cancellation is idempotent and receiver-thread
  joining has a bounded wait.
- Remote JPEG encoding and authenticated record reads now reuse bounded
  per-thread payload buffers. JPEG bytes are written directly behind the frame
  header without an intermediate allocation, receive buffers are exposed only
  after their MAC succeeds, and sustained use of much smaller frames releases
  capacity retained by an earlier extreme frame.
- Remote JPEG decoding now writes ordinary RGB frames directly into a reusable
  client allocation instead of allocating a new decoded image on every frame.
  The viewer retains that lease only while it is the Expose/resize source; old,
  superseded, failed and closed-window frames return it to a best-fit pool of at
  most two free buffers. Each retained buffer is capped at 32 MiB and the pool
  at 64 MiB, while grayscale JPEGs keep their compatible conversion path.
- The shared-memory protocol is v14. JWM and every bar must be rebuilt and
  restarted together, which the existing layout/version validation enforces.
- `display::CANONICAL_LAYOUTS` is now the single source for JWM's layout ids,
  names, symbols, labels and cycle order; `LayoutEnum` derives from it.
- No stable release has been published. The root `0.2.0` manifest version
  remains a development version, not a support commitment.
- CI now treats Clippy correctness, suspicious, and performance diagnostics as
  errors and explicitly tests the Linux action and D-Bus provider adapters.

### Fixed

- The X11 backends no longer report a refused keyboard grab as a success. Both
  `grab_keyboard` implementations discarded the reply's `GrabStatus`, so the
  lock screen's "never display a pretend lock if the exclusive keyboard grab
  failed" guard could not fire.
- Opening a shell panel during a pointer drag cancels the drag instead of
  silently stealing its grab and leaving it armed. The panel's grab replaces the
  drag's and drops motion events, while the motion and button-release handlers
  both bail while a panel is up, so the drag was never committed or cancelled.
- Locking from the Shell Hub's session page no longer leaves the lock marked as
  a page the Hub can be backed out to.
- A transient readback failure inside a scaled `jwm-remote` capture no longer
  retires XRender for the whole session. The accelerated path reads its small
  target back itself, and any error from that read was reported as an XRender
  fault; losing the scaler makes every later frame read back the
  full-resolution drawable and resize it on the CPU, so one transient error
  bought roughly a hundredfold host-CPU increase permanently. Failures are now
  attributed to the stage that failed, and a genuine XRender rejection suspends
  scaling with a 1 s / 5 s / 30 s backoff instead of retiring it outright.
- A single unparsed X11 event no longer disables all four of `jwm-remote`'s
  event-driven capture caches at once. Unknown events are attributed to the
  extension owning their event code and demote only that facility; steady state
  had otherwise gone from one blocking round trip per frame to about five, with
  no path back. Events from extensions the capture connection never selected
  are tolerated in a run of eight.
- `jwm-remote host` declares a 1 KiB inbound record ceiling instead of the
  global 32 MiB frame limit. It only ever receives a hello, empty heartbeats,
  eight-byte acknowledgements and input batches, so an unauthenticated length
  field could previously make it reserve megabytes before any tag was checked.
- A single unauthenticated TCP connection that resets immediately no longer
  kills `jwm-remote host`. The accept loop took the peer address from the
  accepted socket rather than from `accept` itself, and a peer that sent RST
  first made that fail with `ENOTCONN`, which propagated out of the listener.
  Aborted connections, interrupted syscalls and descriptor exhaustion are now
  logged and retried too, so one packet from a port scanner can no longer take
  down the host a user was relying on to reach the machine remotely.
- `jwm-remote` releases held keys and buttons after 600 ms of controller
  silence instead of waiting out the eight-second session idle timeout. The
  host X server generates autorepeat, so a network partition with a key down
  used to type into whatever had focus for the full eight seconds.
- A mouse button the controller sends that the host's pointer map does not
  define — a 12-button mouse, or horizontal-scroll buttons 6 and 7 — is dropped
  instead of ending the session. Because an input batch is validated before
  anything is queued, failing it also discarded every pointer motion and any
  release-all in the same record.
- `jwm-remote` pins a button's physical number when the press is queued, so a
  pointer remap arriving mid-press can no longer release a different button and
  leave the real one stuck down on the host.
- `jwm-remote connect` bounds its TCP connect. Every later phase already had an
  absolute deadline, but a black-holed address burned the kernel's full SYN
  retry budget against a five-second negotiation budget.
- A Composite overlay readback failure now arms the same bounded retry as an
  overlay acquire failure. Re-acquisition otherwise waited for a
  compositor-owner transition that never arrives while the same compositor
  keeps running, so one transient error downgraded the session to ungated root
  capture — and with it the XDamage gate — for the rest of the session.
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
