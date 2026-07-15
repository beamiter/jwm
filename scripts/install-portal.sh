#!/usr/bin/env bash
# Install jwm-portal (xdg-desktop-portal ScreenCast backend) to the user prefix.
# No sudo required; restart xdg-desktop-portal afterwards.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIBEXEC="$HOME/.local/libexec"
DBUS_SVC="$HOME/.local/share/dbus-1/services"
PORTAL_DIR="$HOME/.local/share/xdg-desktop-portal/portals"
PORTAL_CONF="$HOME/.config/xdg-desktop-portal"
PORTAL_MANIFEST="$ROOT/portal/Cargo.toml"
PORTAL_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/portal/target}"
if [[ "$PORTAL_TARGET_DIR" != /* ]]; then
    # Cargo resolves a relative CARGO_TARGET_DIR from the invocation cwd.
    PORTAL_TARGET_DIR="$ROOT/$PORTAL_TARGET_DIR"
fi
PORTAL_BINARY="$PORTAL_TARGET_DIR/release/jwm-portal"
SERVICE_TEMPLATE="$ROOT/portal/dist/jwm-portal.service"
SERVICE_DEST="$DBUS_SVC/org.freedesktop.impl.portal.desktop.jwm.service"

if [[ -n "${JWM_PIPEWIRE_PREFIX:-}" ]]; then
    PIPEWIRE_PKG_CONFIG="$JWM_PIPEWIRE_PREFIX/lib/pkgconfig:$JWM_PIPEWIRE_PREFIX/lib64/pkgconfig"
    export PKG_CONFIG_PATH="$PIPEWIRE_PKG_CONFIG${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
fi

echo "[install-portal] building jwm-portal (release)…"
(
    cd "$ROOT"
    cargo build --locked --release --target-dir "$PORTAL_TARGET_DIR" --manifest-path "$PORTAL_MANIFEST"
)

mkdir -p "$LIBEXEC" "$DBUS_SVC" "$PORTAL_DIR" "$PORTAL_CONF"

install -m755 "$PORTAL_BINARY" "$LIBEXEC/jwm-portal"
install -m644 "$ROOT/portal/dist/jwm.portal"          "$PORTAL_DIR/jwm.portal"
install -m644 "$ROOT/portal/dist/jwm-portals.conf"    "$PORTAL_CONF/jwm-portals.conf"
# xdg-desktop-portal picks the config file by case-sensitive XDG_CURRENT_DESKTOP
# match. jwm-wayland.desktop currently sets DesktopNames=JWM, so install under
# the upper-case basename too — harmless if XDG_CURRENT_DESKTOP=jwm wins instead.
install -m644 "$ROOT/portal/dist/jwm-portals.conf"    "$PORTAL_CONF/JWM-portals.conf"

# D-Bus activation requires an absolute executable path. Render the checked-in
# template with this user's actual libexec path instead of baking in a
# developer-specific home directory.
SERVICE_TMP="$(mktemp "${TMPDIR:-/tmp}/jwm-portal.service.XXXXXX")"
trap 'rm -f "$SERVICE_TMP"' EXIT
printf -v SERVICE_EXEC '%q' "$LIBEXEC/jwm-portal"
SERVICE_EXEC_REPLACED=false
while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" == 'Exec=@JWM_PORTAL_EXEC@' ]]; then
        printf 'Exec=%s\n' "$SERVICE_EXEC"
        SERVICE_EXEC_REPLACED=true
    else
        printf '%s\n' "$line"
    fi
done < "$SERVICE_TEMPLATE" > "$SERVICE_TMP"

if [[ "$SERVICE_EXEC_REPLACED" != true ]]; then
    echo "[install-portal] service template is missing Exec=@JWM_PORTAL_EXEC@" >&2
    exit 1
fi
install -m644 "$SERVICE_TMP" "$SERVICE_DEST"
rm -f "$SERVICE_TMP"
trap - EXIT

# A D-Bus-activated backend stays alive independently of the frontend portal.
# Stop an already-running copy so the next activation loads the binary we just
# installed instead of serving the rest of the session from an old inode.
if command -v pkill >/dev/null 2>&1; then
    pkill -u "$(id -u)" -x jwm-portal 2>/dev/null || true
fi

echo "[install-portal] restarting xdg-desktop-portal user services…"
systemctl --user restart xdg-desktop-portal.service       2>/dev/null || true
systemctl --user restart xdg-desktop-portal-gtk.service   2>/dev/null || true

cat <<EOF
[install-portal] done.

Files installed:
  $LIBEXEC/jwm-portal
  $PORTAL_DIR/jwm.portal
  $SERVICE_DEST
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
