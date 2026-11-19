# Unpublished one-shot winit baseline investigation

Exact source: c994017ba60687c7209e2fc6d451be8745cfdea2 (parent reports includes #122).
Read-only GitHub fetch confirmed this exact tree's native client event format,
get_system_ui shape, winit host-focus handler and existing nested build workflow.
No claim of real reproduction yet. This draft does not modify product or other PRs.

Install mapping, only after approval of a separate Draft:
- alttab-host-focus.yml -> .github/workflows/alttab-host-focus.yml
- probe.py/build.sh/test_probe.py/PLAN.md -> scripts/alttab-host-focus/
No push, master or dispatch trigger. Only matching paths on a Draft pull_request.

Build: pinned checkout, exact second source checkout, official Ubuntu dependency
helper from exact source, pinned existing Rust action, no cache. env -i removes
wrapper/flag/preload/user Git/Cargo settings; fresh HOME/CARGO_HOME, target must
not exist. Both JWM/jwm-tool and native_shell_client are really built --locked,
source is checked clean before/after, hashes recorded then checked by probe.
The normal Draft checkout supplies only reviewed test files, not product builds.

Private Xvfb: explicit :199, -nolisten tcp, random private cookie; no inherited
DISPLAY/Xauthority/buses/home. Validate peer PID+UID+live child+/proc executable,
wrong cookie must fail and private cookie succeed. Require the real Xvfb-created lock with exact owned PID/UID; reject existing
lock/pathname/abstract endpoints before startup. No manufactured lock, no
-displayfd, no manual removal of shared X11 paths. A conflict fails closed. Native clients have no DISPLAY, only private
WAYLAND_DISPLAY. Cleanup only owned process groups, all failures retained.

Positive controls must succeed first: actual IPC window_switcher opens on real
Alt+Tab, Alt release closes and selects B; refocus A only during setup and require
native wl_keyboard B-key receipt from A's owned PID. Then open switcher, focus
owned external Xmessage, release Alt there, focus nested output again. No
Escape/click/clearmodifiers or IPC focus repair after interruption. Record
selector samples, actual IPC selection, native key receipts including exclusion
of nonselected B, and another real Alt-Tab cycle only if all checks passed.

This strict desired-behavior test exits nonzero on regression or setup failure.
A setup/control failure is NOT a reproduced product bug. Artifacts identify
phase and raw commands so reviewers can distinguish the two. An actual residual
selector/native-input failure after both controls is evidence for further
investigation, not license to change lock/security logic.

Only wayland-winit is included. Exact baseline's existing workflow already records
wayland-x11 failure on Xvfb due missing DRI3/DRM (run 44404173276); do not pay for
that known host mismatch. No physical GPU/DRM, lock/unlock, pixel-rendering or
multi-output claim.

Bounds: job75min, dependencies10/toolchain10/contracts2/build40/probe5/artifacts3.
Probe main SIGALRM180s uses a dedicated RuntimeError subtype (not swallowed by
OSError polling); cleanup up to six process groups *6s, hence ~216s plus IO.
Step timeout5min is outer fallback. Always upload evidence, including failed
positive controls; no continue-on-error or expected-failure masking.

Local validation: Python compile, bash syntax, ten offline contracts only.
No Xvfb/real JWM run locally, no cloud network build, no publication. Runtime
compatibility beyond exact API/source checks must be proved on ordinary runner.

Final tightening: host XID searches require exactly one result. A small isolated
Python ctypes helper queries server XRes v1.2 LocalClientPID (not self-reported
_NET_WM_PID); outer must belong to the directly spawned JWM PID, external sink
to the owned xmessage PID, with live process and /proc/exe checks repeated on
every focus change. Private dbus-daemon is spawned separately so JWM PID is
not confused with dbus-run-session. Missing XRes PID is setup failure, never a
property fallback. libxres1 comes from the same official Ubuntu dependency step.
API pattern reference: https://lists.gnome.org/archives/commits-list/2022-March/msg10404.html
Repeat gesture must select B and deliver a fresh actual native b to B but not A,
not merely hide the selector. Host identity helper hash is part of build receipt.

Residual selector gate: observe read-only IPC for a full 2-second window after return,
record elapsed monotonic time with every sample, and require the final three consecutive
inactive observations. All scheduled samples are retained, even after it first hides. A brief queue delay that settles is not labeled a residual
bug. Failure retains the whole timeline; no repair input during observation.

## First real run and minimal fixture correction
Run 44472651354 (head 9556dafc) failed during setup before any Alt-Tab control.
The displayfd-selected :0 pathname disappeared while Xvfb was still live.
Exact baseline Smithay's Xwayland allocation removes a lock-free display's
pathname before binding; the log records its abstract bind collision on :0.
This is not evidence of Alt-Tab residue. The replacement uses normal server
locking on :199; all existing peer PID/UID/exe and cookie checks remain.
No product source changes, no retry of the old head, no claimed runtime pass.
