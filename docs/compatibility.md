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
