# Hardware validation gate (Phase 6)

Hosted CI cannot certify a kernel/GPU/driver combination. Before the first
published `jwm-v*` release, maintainers must run this matrix on real hardware
and keep the results with the release notes (or a private checklist if the
report includes hostnames).

**Daily-drive gate:** `jwm --backend wayland-udev --doctor` must exit 0 with no
blocking errors on every machine in the matrix. Warnings are allowed only when
explicitly accepted in the release notes.

## Machines

Record at least **two GPU vendors**. NVIDIA is strongly preferred as a third
row when available.

| Field | Machine A | Machine B | Machine C (optional) |
| --- | --- | --- | --- |
| GPU vendor / model | | | |
| Driver / Mesa / kernel | | | |
| Distro | | | |
| Outputs (count, mixed DPI?) | | | |
| Doctor exit / notes | | | |
| `jwm-tool perf record` baseline path | | | |

Baselines go under `perf/baselines/` with a complete system label (see
[performance](performance.md)). Never compare unlabeled cross-machine results.

## Scenario checklist

Run on `wayland-udev` unless a row says otherwise. Mark Pass / Fail / Skip.

| # | Scenario | Pass? | Notes |
| --- | --- | --- | --- |
| 1 | Cold start from DM Wayland session | | |
| 2 | Doctor green before and after install | | |
| 3 | Single-monitor modeset + client map | | |
| 4 | Dual-monitor layout + hotplug add/remove | | |
| 5 | Suspend / resume restores session | | |
| 6 | VT switch away and back | | |
| 7 | Interactive screenshot + clipboard PNG paste | | |
| 8 | Screen recording start/stop | | |
| 9 | Lock / unlock | | |
| 10 | Fullscreen game / video (VRR path if panel supports) | | |
| 11 | Control center: audio output switch ≤2 actions | | |
| 12 | Continuous session ≥7 days without forced restart | | |
| 13 | `scripts/test-install-lifecycle.sh` on a clean prefix | | |
| 14 | Upgrade then rollback per [upgrade](upgrade.md) | | |
| 15 | X11 compatibility smoke (`x11rb`) optional | | |
| 16 | Touchpad two-finger scroll: vertical, horizontal, diagonal, lift and restart | | |
| 17 | Pointer stays visible on unequal-height and offset outputs; floating drops adopt the largest overlap | | |
| 18 | Held mouse button across screenshot, launcher, lock and VT changes releases cleanly | | |

For the touchpad row, use a native Wayland client with scrollable content in
both directions. Verify that lifting the fingers ends each active scroll axis
and a new gesture works immediately; normal application kinetic scrolling may
continue after lift. If available, also check a pointing stick's continuous
scroll and a mouse wheel, including high-resolution partial wheel steps.
Nested-backend smoke checks should include a client bound to `wl_pointer`
version 4 to verify ordinary wheel scrolling without newer axis events.

For the multi-output pointer row, move into the gap below a shorter display
with both a mouse and an absolute device when available. Check output edges,
negative output origins, and a disabled display that overlaps an enabled one.
Drop floating windows across a boundary in both directions, including after
keyboard focus moves to a different window, and verify position and tags.

## Perf contract

On each machine that will be cited in release notes:

```bash
jwm-tool perf record --out perf/baselines/<label>.json
jwm-tool perf compare perf/baselines/<label>.json <second-recording>.json
```

Skips must carry reasons. A release that cites performance claims without a
labeled baseline is incomplete.

## Release readiness sign-off

- [ ] Repository **Settings → General → Releases → Enable release immutability**
      (administrator; cannot be set by workflow).
- [ ] Hardware matrix above completed for ≥2 GPU vendors.
- [ ] Doctor gate documented in the changelog.
- [ ] Maintainer checklist in [release-process](release-process.md) finished.
- [ ] Tag `jwm-v<semver>` pushed; assets, checksums, and provenance inspected.

Until these boxes are checked, JWM remains a development build even if the
packaging automation succeeds.
