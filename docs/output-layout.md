# Output layout and framebuffer envelope

JWM's Wayland DRM backend composites into one **global framebuffer** whose size
is the axis-aligned bounding box of every enabled output's origin plus mode
(`proposed_output_framebuffer_size` / `KmsState::total_screen_size`).

## What still works without growing the envelope

Rearranging or disabling outputs is fine when the new layout's bounding box is
**unchanged** (same width and height). Soft-disable, DPMS, and same-size
modesets go through the normal output-management Apply path.

Hotplug that rebuilds the KMS session (`maybe_reinit_kms` → compositor
recreation) also allocates a fresh compositor at the new size.

## Transactional candidate (explicit opt-in)

The default still refuses a live `wlr-output-management` Test/Apply that grows
or shrinks the framebuffer envelope. Set
`behavior.wlr_output_mgmt_allow_modeset=true` only in an independent test session
to exercise the candidate path; the setting now also permits envelope changes
for the existing supported nonnegative, normal-transform topology. Unsupported
rotation/negative-coordinate plans remain rejected by output planning.

Apply preflights the output extent and allocates replacement GLES targets before
changing KMS. DRM TEST_ONLY and the existing modeset transaction then run; only a
successful hardware transaction publishes the prepared compositor targets. A
failed prepare leaves the active targets intact. KMS or publication failures
retire prepared resources and use the existing reverse KMS rollback, with
rollback outcome reported explicitly. Capture/recording targets and input/window
geometry are synchronized after publication. An unfinished rollback remains a
failure, never success inferred from a resource allocation.

Offscreen GLES tests cover resource prepare/cancel/publish. They do not prove a
monitor stays visible during a real modeset or rollback. Mixed refresh rates,
negative origins, rotations, fractional scale, recording, lock/VT transitions,
unplug and allocation failure still need hardware evidence before enabling this
by default. The operator countdown wrapper below adds a separate confirmation/revert flow;
the low-level transaction itself does not display confirmation UI.

## Workarounds

1. Prefer layouts that keep the same bounding box.
2. For a true size change: restart the session, or trigger a full KMS reinit
   (e.g. seat/VT cycle that rebuilds outputs), then re-apply the layout.
3. Tools like kanshi that expand the envelope need that restart/reinit step.

Tracked as gap 6 in [sota-gap-queue](sota-gap-queue.md). The staged
candidate covers resource preparation and publication; hardware stress and
failure-path acceptance remain open.

`jwm-tool health` prints compact `monitors` for output layout operators.

## Interactive display preview

With a `wlr-randr` version supporting `--json` (the wrapper follows the upstream
[v0.5.0 JSON and CLI contract](https://gitlab.freedesktop.org/emersion/wlr-randr/-/blob/v0.5.0/main.c)):

```sh
jwm-display-preview --timeout 15 -- --output DP-1 --mode 1920x1080@60Hz --pos 0,0
jwm-display-preview --snapshot   # read-only; allowed without a TTY
```

Enter confirms; Esc, timeout or interrupt restores and verifies the prior
settings. Before applying, the wrapper validates a complete snapshot and runs
`--dryrun`. It restores only named outputs, and only while the complete observed
layout, physical identity fields and mode catalogs still match the preview.
An observed concurrent change, unplug or replaced identity ends its ownership
without overwriting that change. Failure or unverified rollback is explicit.
Catchable interrupts trigger this cleanup; SIGKILL or a host failure cannot run
a client-side rollback. The compositor transaction still owns allocation/KMS
failure handling independently of this operator helper.

**Boundary:** upstream JSON omits the protocol serial and head object generation.
Therefore a short unplug/replug with identical identity between polls, or another
client applying between the final equality check and the restore command, cannot
be excluded by this CLI wrapper. It is conservative snapshot checking, not atomic
compare-and-swap or proof of a physical safe restore. A future persistent protocol
client can retain the actual serial for an atomic stale-configuration rejection.

Only explicit advertised modes, positions, scales, transforms, adaptive-sync
values and on/off flags are accepted. Supply refresh rate when a resolution has
multiple advertised rates. `--off` is the only setting allowed for its named head
in that preview. An initially-disabled head cannot be enabled here because its
prior geometry is absent from the upstream JSON and would not be restorable.
The upstream `wlr-randr --json` mode refresh is in **Hz**, even though the
underlying protocol uses mHz. The wrapper's optional diagnostic report contains
normalized `mode: [width, height, refresh_mHz]` tuples; its restore command converts
that last value back to Hz. Do not pass the normalized integer as a Hz argument.
Older distribution packages without `--json` fail before changes; install a
compatible `wlr-randr` first. This task tests the wrapper against a fake CLI and
private TTY only; it does not invoke output changes on the working desktop.
