# Portal broker acceptance probe

An isolated test of **actual xdg-desktop-portal dispatch**, not a display
manager, user systemd session, UI, or capture test. The baseline is
`0631f2639676e6dc1cc4d1c81cdad8d0a2804e17`. The probe reads four exact files
from Git objects, not the working tree. The workflow is limited to an isolated
same-repository Draft with base `dot/portal-broker-baseline-0631` pointing at
that SHA and head `dot/portal-broker-acceptance`. Keep the workflow and fixture
files in a single commit directly atop the fixed baseline. No production
configuration, installer, smoke script or compositor changes are required.

Run as a non-root user on a disposable Ubuntu 24.04 machine with distro
`dbus-daemon`, `python3-gi`, `xdg-desktop-portal`, and
`xdg-desktop-portal-gtk` packages already installed. The probe does not invoke
systemctl, connect to the current session bus, read the current desktop, start a
screen capture, or launch a chooser window. The hosted workflow installs only its
named packages using the repository's official-signed-APT helper.

    /usr/bin/python3 -I -B scripts/portal-broker-acceptance/test_portal_probe.py -v
    env -i PATH=/usr/bin:/bin LANG=C.UTF-8 timeout --kill-after=3 120 \
      /usr/bin/python3 -I -B scripts/portal-broker-acceptance/portal_probe.py \
      --repo "$PWD" --output /tmp/new-portal-evidence

## What counts as evidence

- Cold X11, cold Wayland, and both same-user transitions use fixture-provided
  session variables and an actual session bus. Observers record only five
  selected display/session variables on fresh activation. The no-import
  observation and present-only update use separate newly activated observers.
  A foreign desktop identity is also tested. These are **modeled inputs**, not
  samples of an actual display manager's login scripts.
- The upstream D-Bus daemon activation environment can add or override keys but
  cannot unset keys. An absent new-session variable can therefore leave its old
  value in a surviving private bus. Systemd user execution state and an
  already-running broker are separate and not tested here.
  [Upstream D-Bus documentation](https://dbus.freedesktop.org/doc/dbus-update-activation-environment.1.html#limitations)
- The real broker is activated through a private D-Bus service file, with bus
  peer PID/UID and broker owner PID/executable checked. GTK-name FileChooser
  and JWM-name ScreenCast are deliberate backend fixtures. A persistent
  frontend client checks the matching request path and cancellation signal,
  plus the actual method receipt from the expected backend PID. No raw
  package-installed backend is started by the test's restricted service dirs.
- The explicit-route positive control intentionally uses the upstream test
  directory override and an explicit GTK mapping in a scratch copy. The next
  controlled run uses the exact config, then stock discovery runs have
  **no override**, with the descriptor only in the installed user-data layout.
  Thus a missing ScreenCast export is not conflated with failed fixture setup.
  A negative observation is not by itself evidence that an older supported
  distro is broken. Read the broker version/package and verbose log first.
- `result.json` records exact input hashes and separates setup error, method
  not confirmed, not exported, and receipt-confirmed. Missing prerequisites,
  positive-control failure, or a request with no valid receipt exits nonzero;
  later cases not reached remain marked not-run. `observed-routing` means
  observations were collected, not that every interface must be exported.
  `--activation-only` is an optional bus-only diagnostic, not Portal acceptance.

The 1.18.4 source loads implementation metadata from its compiled data directory
unless the test override is set. The legacy GTK fallback is separate from this
discovery rule. The 1.20.1 upstream change added user and other XDG data paths;
always use the actual distro binary because it may have backports.
[Original metadata loader](https://github.com/flatpak/xdg-desktop-portal/blob/1.18.4/src/portal-impl.c#L254-L267)
· [Legacy chooser fallback](https://github.com/flatpak/xdg-desktop-portal/blob/1.18.4/src/portal-impl.c#L558-L621)
· [Newer discovery paths](https://github.com/flatpak/xdg-desktop-portal/pull/1665/files).

## Not covered

Native GTK UI and generic OpenURI are not part of this first gate. For OpenURI
the implementation interface is AppChooser; a separate safe test needs a local
fixture-only URI scheme, multiple inert candidate applications, explicit
chooser request and a no-launch assertion.

Actual X11/Wayland login, persistently running user systemd/broker state and
capture each need separate evidence. For an owned 24-bit Xvfb positive control,
`src/remote/window_capture_integration.rs` tests the native X11 path with
`JWM_REMOTE_TEST_DISPLAY` explicitly set to that verified server. Do not use an
ambient DISPLAY. Native X11 pixel success is not Portal ScreenCast success:
the production JWM backend in `portal/src/capture.rs` requires a reconnectable
Wayland display and compositor capture protocol. No real display is connected
by this fixture.
