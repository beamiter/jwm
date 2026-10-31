# Eva Companion bridge and compositor node

This implementation is opt-in and does not change Alt+Ctrl+E, Eva's configured
provider, approvals, desktop executor, wake settings, microphone ownership, or
terminal PTYs. It does not start a second Realtime session.

For experimental GTK and patched Vulkan/native X11 frame acknowledgements, see
[extended X11 frame feedback](x11-frame-feedback.md). That separate opt-in path
does not upgrade the compositor body's retirement capability described below.

## Voice observation

Set `JWM_EVA_COMPANION=1` on the **existing** `jwm-eva daemon`. It connects to
`$XDG_RUNTIME_DIR/jcompanion/eva.sock` (override `EVA_COMPANION_SOCKET`), bound to
`XDG_SESSION_ID`. SSH and missing graphical session information are rejected.
The companion daemon must already be running. No new credentials are required.

- Before the existing voice activation probes/configures Realtime, a correlated
  VoiceAcquire handshake must succeed. The coordinator denies it while a text
  response is in flight and prevents text starts while the lease is held.
  Timeout/unavailable coordinator fails closed in opt-in mode. Resource owners
  drop before VoiceRelease; IPC loss does not imply audio cleanup and retains
  the durable lease for explicit recovery. Existing wake/idle connections are
  unchanged; this gate is for the primary voice activation.
- An independent bounded worker connects using the exclusive Eva observer role;
  that role cannot acquire a body or invoke desktop actions.
- Capture pose follows verified, recent locally framed PCM activity, not WebSocket
  readiness. Only numeric timing/state metadata crosses the audio boundary; no
  PCM is copied into IPC. Delayed/unverified frames are not given a new timestamp
  merely because they were processed later.
- Speaking requires recent native PipeWire/PulseAudio playback-position evidence.
  Queued/generated PCM counts and played sample counts are separate. Unknown
  native timing remains `None`, never an estimated playback position.
- Playback completion, local interruption, session errors and resource release
  produce distinct observations. Multiple output segments in one turn accumulate
  counters rather than resetting the ledger.
- The bridge exports numeric identity/counters/state only: no transcript, raw
  audio, model credentials, window title, authorization token or action payload.
- IPC loss invalidates observations and reconnects at a bounded 250ms–5s
  cadence using a stable random identity for the same live Eva process. Old
  audio turns are not replayed. Only matching resource-cleanup releases are
  retried until their correlated VoiceReleased receipt; an uncertain acquisition
  is not mislabeled as a definite refusal. Queue overflow remains fail-closed.
  A fresh Eva process has a fresh source nonce and cannot impersonate or clear a
  previous process's durable lease. Explicit operator proof/recovery for a
  permanently orphaned lease is not yet implemented: keep new voice/text work
  blocked. Do not clear the journal or use heartbeat expiry/PID reuse as proof.
- These device facts are not task completion receipts. Interrupting speech does
  not grant permission to stop a build or reverse a desktop operation.

### Voice turn ledger

The coordinator now records accepted voice observations in the same bounded,
live ledger used for text turns. Voice records retain the companion identity,
original Eva source, activation lease and turn ID; they have no inferred terminal,
pane, project or task context. Text and voice IDs occupy separate namespaces.
Generated samples, the currently reported playback position and the last confirmed
playback lower bound are distinct. An unavailable clock clears current playback
evidence without erasing samples already confirmed as played. Sample counts are
not converted to milliseconds without a negotiated rate.

Interruption, observation loss, resource release and transition to the next voice
turn have different outcomes. None marks a desktop task successful or records
generated-but-unplayed output as heard. Completed source/turn identities remain
replay tombstones for the coordinator process lifetime. The shared 1024-entry
bound blocks new activations/context requests while still allowing cleanup of an
existing lease.

The coordinator's private profile also keeps a bounded `dialogue-index.json`.
Turn boundaries persist metadata; frequent audio-position updates remain in
memory until the next boundary. A coordinator restart marks unfinished historical
turns `restarted_unknown`, retains only committed played lower bounds, and clears
current audio facts. Historical entries never recreate an active turn, release
the independent durable voice lease, or replay speech. Corrupt/unwritable history
is preserved and prevents new model admission; existing ownership cleanup remains
available. Task references retain their registered source/project even after the
visible pane changes. They are references to host observations, not executor proofs.

Text-route input/replies stay in volatile session memory. They are not persisted
or automatically included in another model request. The voice bridge still carries
no transcripts. Explicit content retention/sharing UI, full dialogue/tool/result
integration and physical audio acceptance remain separate work.

Each activation also has an Eva-process-local checked sequence, so a restarted
WM reusing its generation cannot reuse an old release identity. Existing releases
still retry their original exact ID. Counter exhaustion blocks new activation.

`EVA_COMPANION_DEBUG_VOICE=1` on the coordinator logs metadata-only ledger records
for diagnostics. It does not include transcripts, PCM, model input, task content,
credentials or desktop action permissions. No microphone or provider is started
by this option.

### Clocked observations

The source bundle in `eva/vendor/companion-voice` negotiates the explicit
`linux_boottime_ns_v1` extension before publishing audio state. Public client and
protocol Git pins remain fixed. Generate/check the bundle from the companion
workspace with `python3 scripts/vendor-voice-adapter.py WORKSPACE_ROOT [--write]`.
An old coordinator cannot silently accept the new guarantee; unclocked audio on
the updated coordinator cannot claim a current listening/speaking pose.

The coordinator accepts observations at most 250 ms old in the common Linux
boot-time clock, rejects future/reordered data and suppresses expired pose facts.
Active listening/speaking observations refresh at a bounded 80 ms cadence.
The timestamp belongs to the verified capture or playback observation, not the
later socket write. An expired local observation is discarded without resetting
the healthy connection or closing its turn. Transport buffering and process
pauses therefore do not renew an old pose; fresh observations can restore the
same live turn. Expiry never releases the durable voice lease, cancels a task,
starts another model request or claims that audio devices were released.

These bounds are freshness policy, not measured physical audio latency or full
suspend/resume acceptance. The native PipeWire/PulseAudio checks use private
synthetic sources and silent sinks. Physical microphone/speaker and acoustic
barge-in acceptance remain outstanding.

## Shared profile and consented memory

Every primary voice activation retrieves the coordinator's persistent
companion ID, edited name and profile revision. Before **each** planning or
spoken feedback request the bridge refreshes this snapshot. The payload is a
bounded `authorized_companion_data` **user-role data item**, never a privileged
instruction, execution permit, tool argument or approval.

`JWM_EVA_GLOBAL_MEMORY=1` explicitly permits up to four already-confirmed global
memory entries (server limit 4096 bytes). Default: no memory entries. Task/project
memory and raw terminal content are never inferred from the focused window.
Deletion/pause is reflected by the next request's fresh snapshot; data already
sent to a provider cannot be retroactively withdrawn. Context timeout/denial
blocks new primary generation rather than using stale memory. Credentials stay
in the existing Eva provider configuration.

## Desktop lock and actual resume observation

Set `JWM_COMPANION_OBSERVER=1` for JWM in native-body mode; scene mode also starts
this independent observer. The coordinator requires
`EVA_COMPANION_DESKTOP_EXE=/absolute/path/to/jwm` and verifies that executable
against the kernel-authenticated peer PID. Other frontend roles cannot publish
lock/resume events. Same-UID hostile processes remain outside strong isolation.

The worker publishes actual JWM/Wayland lock state with `SessionLock`, and uses
the Linux `CLOCK_BOOTTIME - CLOCK_MONOTONIC` difference to detect suspend/resume.
It rejects delayed clock samples; an `Instant` loop gap is not called a resume.
On reconnect it sends current lock state before Resume. Unknown/disconnected
authority is not proof of unlock. Scene mode hides locally while preserving the
same node authorization across temporary lock; actual transfers still require
retirement evidence. No new model request is authorized while locked/unknown.

## Optional one-node desktop renderer

Run the coordinator in its persisted `--compositor-only` mode, then start JWM
with `JWM_COMPANION_SCENE=1`. The coordinator denies native-host body grants in
this mode. **Before initial deployment, stop/disable old independent terminal
pets. An unregistered old client cannot be remotely certified absent.** A
previous possible residency in the durable journal is not cleared by this flag,
by heartbeat expiry, or by JWM registration.

The one compositor host negotiates an initial grant only from the coordinator's
clean residence state. Native retirement capability remains **None**: this code
never calls a submitted GL draw, status heartbeat or window callback a presented
retirement fence. After an uncertain revoke/disconnect, strict transfer remains
blocked. In compositor-only mode Hidden temporarily suppresses the same
already-authorized node; Recall may reveal that exact token, never invent a new
grant from a snapshot. Restart/disconnect still blocks uncertain ownership.
Do not delete a pending journal to work around this safety condition.

The renderer has actual, separate implementations for X11 GL, Wayland DRM/GLES,
and nested Wayland X11/Winit memory elements. They consume the same immutable
atlas RGBA snapshot, not a model response or per-frame IPC ASCII stream. CPU
raster preparation and IPC run on the worker; the render path only uploads a
changed texture and draws one node. Static/hidden frames do not schedule repeated
body redraws. Rendering is click-through and cannot grab keyboard focus.

Current safe placement is an **exposed desktop corner on the selected monitor**,
using live work-area geometry and rejecting every overlapping client rectangle.
It hides for full-screen clients, modal system UI/lock, unavailable outputs,
insufficient space, coordinator loss and changed target/workspace. Old pixels
are explicitly damaged on node replacement/removal. A visible node requires
composition, so it cannot be skipped by a direct-scanout route. A later target
cannot inherit an old task binding.

`get_companion_desktop_state` is a new read-only JWM query: stable WM/focus/monitor
identities, workspace masks, work areas, lock/fullscreen facts and honest
capabilities. It intentionally omits window titles, commands and action permits.

### Deliberately not claimed complete

- Authenticated terminal-native-window anchor mapping is not implemented. The
  renderer does not guess mappings from titles or PID and will hide rather than
  paint over tiled terminal input/selection. Desktop-safe placement is real;
  “follows terminal safe anchors” is not yet a supported claim.
- Compositor retirement fences / scanout confirmation and cross-native recovery
  still need actual backend integration and evidence. X11 and Wayland capabilities
  are independently unverified. No `Retired` or `Presented` message is fabricated.
- The body is currently click-through; independent companion input/card handling
  belongs to the coordinator/native GUI integration, not shell command injection.
- Microphone, speaker, barge-in latency, GPU rendering, multi-output/HiDPI,
  direct-scanout, lock-screen and native screenshot acceptance require an
  authorized physical/session test. Deterministic tests are not that evidence.

## Checks

`cargo test -p jwm_eva_core companion` exercises fail-closed backend policy,
retirement-vs-heartbeat, lock/fullscreen/output suppression, epoch and target
invalidation with deterministic fixtures.

`cargo test -p jwm --lib --no-default-features companion_scene` exercises the
actual CPU atlas raster and safe desktop placement, with no native display.

`cargo test --manifest-path tests/companion_bridge_fixture/Cargo.toml` compiles
exact production observer/scene/font source against explicit no-device and
no-notifier fixture stubs. It tests privacy, profile bounds, reconnect cadence,
clock-offset policy, stale grants and actual CPU atlas raster dimensions. It
is not IPC/device/scanout validation.

`cargo test -p jwm-eva --lib companion` exercises bridge identity and actual-fact
state separation. Building this crate requires PipeWire/SPA and PulseAudio dev
libraries; running these fixture tests does not invoke audio devices or an API.

Compile X11 and Wayland features independently. A passing no-backend build does
not validate either native draw path. Live Unix-socket and desktop tests must not
be represented as passed when the executor does not permit them.

### Recorded build evidence (2026-10-30)

- `jwm_eva_core companion`: 4 deterministic tests passed.
- Exact-source bridge fixture: 35 deterministic tests passed.
- `cargo check -p jwm-eva --offline`: passed with scoped native sysroot.
- `cargo check -p jwm --no-default-features --features backend-wayland-nested
  --offline`: passed, covering both nested renderer integrations.
- `cargo check -p jwm --no-default-features --features backend-x11rb
  --offline`: passed after official scoped `libxxf86vm-dev` was provisioned.
- `cargo check -p jwm --no-default-features --features backend-wayland-udev
  --offline`: passed after official locked crate fetch and scoped `libseat-dev`
  provisioning; includes the final retained-alpha composition guard.
- Actual CPU raster preview inspected; not a desktop/GPU screenshot.
- Live Unix sockets are denied by this execution environment. No real audio,
  model call, GPU session, lock-screen or retirement acceptance run is claimed.

Detailed transient logs live outside the repository under
`eva-workspace/recovery-evidence/jwm-companion/`; no build outputs are shipped
with the independent fixture.

## Immutable source release verification

Companion client/protocol/atlas are pinned to public `beamiter/jcompanion`
`9aa569540091524d637a7115b64c587b9177a55b` (protocol 1). Cargo fetches that
exact source; no sibling companion or atlas directory is required. Final checks
using `--locked --offline` passed: bridge fixture 35/35, Eva policy 4/4,
Eva production check, X11rb, nested Wayland and DRM production checks.
These are compilation and synthetic protocol/policy evidence, not live hardware,
compositor scanout, microphone/speaker, IME or billed provider acceptance.
