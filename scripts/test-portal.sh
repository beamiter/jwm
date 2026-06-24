#!/usr/bin/env bash
# Smoke-test for jwm-portal. Run after `scripts/install-portal.sh` AND with a
# jwm Wayland session active (XDG_CURRENT_DESKTOP=jwm). Exercises the parts we
# can poke without a full SDK client.
#
# Exit codes:
#   0 — all checks passed
#   1 — at least one check failed (see stderr for which)
#   2 — environment unusable (no D-Bus, no session bus address, etc.)

set -uo pipefail

PASS=0
FAIL=0

ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; PASS=$((PASS+1)); }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*" >&2; FAIL=$((FAIL+1)); }
step() { printf '\n=== %s ===\n' "$*"; }

if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
  echo "no DBUS_SESSION_BUS_ADDRESS; can't run" >&2
  exit 2
fi

BUS=org.freedesktop.impl.portal.desktop.jwm
PATH_OBJ=/org/freedesktop/portal/desktop
IFACE=org.freedesktop.impl.portal.ScreenCast

step "1. Activate the backend (D-Bus auto-launch)"
if gdbus call --session --dest "$BUS" --object-path "$PATH_OBJ" \
    --method org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then
  ok "$BUS is responsive"
else
  bad "$BUS did not answer Ping — is jwm-portal installed and the .service file present?"
  echo "    expected: ~/.local/share/dbus-1/services/${BUS}.service" >&2
fi

step "2. Introspect the ScreenCast backend interface"
introspect=$(gdbus introspect --session --dest "$BUS" --object-path "$PATH_OBJ" 2>&1)
if [[ $? -ne 0 ]]; then
  bad "introspection failed: $introspect"
else
  for method in CreateSession SelectSources Start OpenPipeWireRemote; do
    if grep -q "$method" <<<"$introspect"; then
      ok "method $method advertised"
    else
      bad "method $method NOT advertised"
    fi
  done
  for prop in version AvailableSourceTypes AvailableCursorModes; do
    if grep -q "$prop" <<<"$introspect"; then
      ok "property $prop advertised"
    else
      bad "property $prop NOT advertised"
    fi
  done
fi

step "3. Confirm xdg-desktop-portal routes ScreenCast to us"
# Frontend interface lives on org.freedesktop.portal.Desktop. We poke the
# session-bus name and check that the user's portal config points at jwm.
conf=~/.config/xdg-desktop-portal/jwm-portals.conf
if [[ -f "$conf" ]] && grep -q '^org.freedesktop.impl.portal.ScreenCast=jwm' "$conf"; then
  ok "$conf maps ScreenCast → jwm"
else
  bad "$conf missing or ScreenCast not routed to jwm"
fi

portal_file=~/.local/share/xdg-desktop-portal/portals/jwm.portal
if [[ -f "$portal_file" ]] && grep -q "DBusName=$BUS" "$portal_file"; then
  ok "$portal_file advertises $BUS"
else
  bad "$portal_file missing or DBusName mismatch"
fi

step "4. rpath sanity (no LD_LIBRARY_PATH at runtime)"
bin=~/.local/libexec/jwm-portal
if [[ -x "$bin" ]]; then
  pw_dep=$(ldd "$bin" | awk '/libpipewire-0.3/{print $3}')
  if [[ "$pw_dep" == /opt/pipewire-1.2/* ]]; then
    ok "libpipewire resolved via rpath to $pw_dep"
  else
    bad "libpipewire resolves to $pw_dep (expected /opt/pipewire-1.2/...)"
  fi
else
  bad "$bin not installed"
fi

step "5. CreateSession round-trip"
# Random handles — we don't intend to keep the session, only verify the call
# completes with response=0.
SH="/org/freedesktop/portal/desktop/session/jwm_test_$$"
RH="/org/freedesktop/portal/desktop/request/jwm_test_$$"
out=$(gdbus call --session --dest "$BUS" --object-path "$PATH_OBJ" \
        --method "${IFACE}.CreateSession" \
        "objectpath '$RH'" "objectpath '$SH'" "''" "{}" 2>&1)
if grep -q '(uint32 0' <<<"$out"; then
  ok "CreateSession returned response=0"
else
  bad "CreateSession unexpected output: $out"
fi

step "6. Session interface served at session_handle"
# After CreateSession we expect the backend to expose
# org.freedesktop.impl.portal.Session at $SH, with a Close method.
introspect_sh=$(gdbus introspect --session --dest "$BUS" --object-path "$SH" 2>&1)
if grep -q 'org.freedesktop.impl.portal.Session' <<<"$introspect_sh"; then
  ok "Session interface advertised at $SH"
else
  bad "Session interface NOT advertised at $SH (introspect: $introspect_sh)"
fi

step "7. Session.Close tears down cleanly"
close_out=$(gdbus call --session --dest "$BUS" --object-path "$SH" \
              --method org.freedesktop.impl.portal.Session.Close 2>&1)
if [[ -z "$close_out" || "$close_out" == "()" ]]; then
  ok "Session.Close succeeded"
else
  bad "Session.Close unexpected output: $close_out"
fi
# Post-close: the interface should have been removed from the bus.
post_close=$(gdbus introspect --session --dest "$BUS" --object-path "$SH" 2>&1 || true)
if ! grep -q 'org.freedesktop.impl.portal.Session' <<<"$post_close"; then
  ok "Session interface removed after Close"
else
  bad "Session interface still present after Close"
fi

echo
echo "Results: $PASS pass, $FAIL fail"
if (( FAIL > 0 )); then
  echo
  echo "Manual follow-up (requires real client + jwm session):"
  echo "  XDG_CURRENT_DESKTOP=jwm chromium --enable-features=WebRTCPipeWireCapturer"
  echo "  XDG_CURRENT_DESKTOP=jwm obs"
  exit 1
fi
exit 0
