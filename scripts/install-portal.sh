#!/usr/bin/env bash
# Install jwm-portal (xdg-desktop-portal ScreenCast backend) to the user prefix.
# No sudo required; restart xdg-desktop-portal afterwards.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIBEXEC="$HOME/.local/libexec"
DBUS_SVC="$HOME/.local/share/dbus-1/services"
PORTAL_DIR="$HOME/.local/share/xdg-desktop-portal/portals"
PORTAL_CONF="$HOME/.config/xdg-desktop-portal"

echo "[install-portal] building jwm-portal (release)…"
cargo build --release -p jwm-portal --manifest-path "$ROOT/Cargo.toml"

mkdir -p "$LIBEXEC" "$DBUS_SVC" "$PORTAL_DIR" "$PORTAL_CONF"

install -m755 "$ROOT/target/release/jwm-portal" "$LIBEXEC/jwm-portal"
install -m644 "$ROOT/portal/dist/jwm.portal"          "$PORTAL_DIR/jwm.portal"
install -m644 "$ROOT/portal/dist/jwm-portal.service"  "$DBUS_SVC/org.freedesktop.impl.portal.desktop.jwm.service"
install -m644 "$ROOT/portal/dist/jwm-portals.conf"    "$PORTAL_CONF/jwm-portals.conf"
# xdg-desktop-portal picks the config file by case-sensitive XDG_CURRENT_DESKTOP
# match. jwm-wayland.desktop currently sets DesktopNames=JWM, so install under
# the upper-case basename too — harmless if XDG_CURRENT_DESKTOP=jwm wins instead.
install -m644 "$ROOT/portal/dist/jwm-portals.conf"    "$PORTAL_CONF/JWM-portals.conf"

echo "[install-portal] restarting xdg-desktop-portal user services…"
systemctl --user restart xdg-desktop-portal.service       2>/dev/null || true
systemctl --user restart xdg-desktop-portal-gtk.service   2>/dev/null || true

cat <<EOF
[install-portal] done.

Files installed:
  $LIBEXEC/jwm-portal
  $PORTAL_DIR/jwm.portal
  $DBUS_SVC/org.freedesktop.impl.portal.desktop.jwm.service
  $PORTAL_CONF/jwm-portals.conf

Verify:
  gdbus introspect --session \\
    --dest org.freedesktop.impl.portal.desktop.jwm \\
    --object-path /org/freedesktop/portal/desktop

  XDG_CURRENT_DESKTOP=jwm chromium --enable-features=WebRTCPipeWireCapturer
  XDG_CURRENT_DESKTOP=jwm obs

Env overrides for the MVP auto-picker:
  JWM_PORTAL_OUTPUT=<output-name-or-substring>
  JWM_PORTAL_WINDOW=class:<app_id>      # or title:<substring>
EOF
