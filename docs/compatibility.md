# Compatibility and support

JWM is under active development and has no stable published release. This is the
current development/testing contract, not a production-support promise.

## Platform matrix

| Surface | Current status | Important gaps |
| --- | --- | --- |
| X11RB | Primary development backend; integrated compositor | Inherits X11's session-wide trust model; server/driver extensions vary |
| XCB | Differential policy coverage with X11RB | Parity tests cannot cover every server, extension, or GPU |
| Wayland DRM/KMS | Direct-session development backend | Needs DRM/GBM/EGL, input, seat permissions, and real-hardware validation |
| Nested Wayland | CI/development smoke backends | Not a production DRM/KMS substitute; capture is absent where unsupported |
| XWayland | Available in Wayland sessions | Inherits X11 isolation limits and application/driver quirks |
| Portal/bars | Optional, separate components | Portal needs PipeWire 1.2 metadata; some toolkit bars have narrower gates |

The binary-bundle design currently targets **x86_64 Linux built on Ubuntu
22.04**. The host must provide compatible graphics, input, seat, audio, D-Bus,
and font libraries. Other distributions/architectures should build the tagged
source with Rust 1.89 or newer. No binary promise exists for ARM, musl, BSD,
macOS, or Windows.

Hosted CI cannot certify a kernel/GPU/driver combination. Run `jwm --backend
wayland-udev --doctor` and validate modeset, hotplug, suspend/resume, VT switch,
multi-monitor, capture, and rendering on real hardware. HDR remains fail-closed
where output coherence is not guaranteed; VRR, direct scanout, color management,
and EGL/GBM behavior remain driver-sensitive.

## Variable refresh rate (VRR)

`[behavior]` carries `vrr_enabled` (default true), `vrr_min_fps` (30),
`vrr_max_fps` (240), and `game_classes` (window classes treated as games). What
they do depends on the backend:

- **Wayland DRM/KMS:** a per-output presentation policy runs once per frame.
  `VRR_ENABLED` is asserted while a mapped fullscreen window *covers that
  output* and cleared otherwise; VRR on a static desktop makes some panels
  flicker, so it follows the content. It follows only the content: a cursor
  moving over the game, an overlay, or a colour-delivery retry do not change
  it, because each change costs a mode-size test buffer and a test commit and
  is a visible refresh renegotiation on many panels.

  It is programmed through Smithay's own `use_vrr`, only on connectors that
  report VRR can change without a modeset, and only when the value differs
  from the last one attempted — a driver that refuses a value is not asked
  again until the wanted value changes. `set_vrr_enabled` over IPC latches an
  override the policy reads rather than programming the hardware directly;
  without that the next rendered frame would recompute VRR from content and
  undo the request.

  (Before this, VRR was written straight onto the CRTC property at output
  init and by `set_vrr_enabled`, and never survived: Smithay re-asserts its
  own cached VRR value in every atomic request it builds, so the enable was
  undone by the very next page flip while the IPC call reported success.)

  `wp_tearing_control_manager_v1` is published when
  `wayland_enable_tearing_control` is on (default true). Hints are
  double-buffered and latched at `wl_surface.commit`, and the same policy
  decides per output whether the frame would be flipped asynchronously —
  but it never is, and the reason is reported rather than implied: JWM's
  frame submission goes through Smithay's `DrmCompositor::queue_frame`, whose
  submission step hardcodes its atomic commit flags and offers no way to
  request `PAGE_FLIP_ASYNC`. `jwm-tool get_tearing_hints` and
  `render_decisions.tearing` name that blocker
  (`submission_cannot_request_async_flip`) per output, alongside the ones
  that would apply anyway: a driver without
  `DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP`, a frame that needs composition, a
  colour-delivery retry, or pending surface state that forces a modesetting
  commit.
- **X11:** the X server owns DRM master, so no per-output VRR toggle is
  reachable through RandR or any X extension; `set_vrr_enabled` therefore
  returns an explicit "unsupported" error instead of pretending. The flags only
  drive the HUD/metrics "VRR active" indicator. Real VRR for games comes from
  `fullscreen_unredirect` (default true) letting the game present directly,
  plus driver-side configuration such as amdgpu's `VariableRefresh` xorg.conf
  option.

## Independent component SemVer

The root `jwm`, bridge, portal, shared protocol, bar core/providers, and every
bar own their SemVer number. `jwm-v0.2.0` names a root JWM release and exact
source bundle; it does not imply every component is version 0.2.0. Before a
component reaches 1.0, minor versions may break compatibility. After 1.0,
breaking public changes require a major version.

## Schema and deprecation policy

- **Configuration:** additive keys with defaults are compatible. Renames,
  removals, type/meaning changes, or stricter validation require a warning and
  continued loading of the old form for at least one JWM minor-release cycle.
- **IPC:** additive commands/topics/fields are compatible and clients must
  ignore unknown object fields. Removing or changing commands, required
  arguments, response meaning, or events requires a warning/capability signal
  for at least one minor cycle. Query `jwm-tool capabilities --json`.
- **Versioned JSON:** incompatible envelope/field changes increment
  `schema_version`; readers must reject unsupported future versions.
- **Sessions:** JWM reads the current and at least previous format through a
  tested in-memory migration. It does not rewrite the old file until a later
  atomic save. A format is removed only after at least two minor cycles and the
  changelog identifies the last reader.

Cycle counts begin with the first published release introducing a deprecation.
Urgent security removals may shorten a window, but require an advisory,
changelog entry, and migration or disablement path.

Report failures with exact component versions, backend, distribution, kernel,
GPU, driver, and renderer. Review support bundles before sharing them; use the
private process in [SECURITY.md](../SECURITY.md) for sensitive failures.
