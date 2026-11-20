#!/usr/bin/env bash
# Install jwm-portal (xdg-desktop-portal ScreenCast backend) to the user prefix.
# No sudo required; restart xdg-desktop-portal afterwards.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIBEXEC="$HOME/.local/libexec"
DATA_HOME=${XDG_DATA_HOME:-$HOME/.local/share}
CONFIG_HOME=${XDG_CONFIG_HOME:-$HOME/.config}
[[ $DATA_HOME == /* ]] || DATA_HOME="$HOME/.local/share"
[[ $CONFIG_HOME == /* ]] || CONFIG_HOME="$HOME/.config"
DBUS_SVC="$DATA_HOME/dbus-1/services"
PORTAL_DIR="$DATA_HOME/xdg-desktop-portal/portals"
PORTAL_CONF="$CONFIG_HOME/xdg-desktop-portal"
PORTAL_MANIFEST="$ROOT/portal/Cargo.toml"
PORTAL_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [[ "$PORTAL_TARGET_DIR" != /* ]]; then
    # Cargo resolves a relative CARGO_TARGET_DIR from the invocation cwd.
    PORTAL_TARGET_DIR="$ROOT/$PORTAL_TARGET_DIR"
fi
PORTAL_BINARY="$PORTAL_TARGET_DIR/release/jwm-portal"
SERVICE_TEMPLATE="$ROOT/portal/dist/jwm-portal.service"
SERVICE_DEST="$DBUS_SVC/org.freedesktop.impl.portal.desktop.jwm.service"
PORTAL_METADATA="$ROOT/portal/dist/jwm.portal"
NOBLE_PORTAL_METADATA=/usr/share/xdg-desktop-portal/portals/jwm.portal

# The tested Ubuntu 24.04 1.18.4-1ubuntu2.24.04.x update family does not
# discover per-user .portal metadata. Refuse before building, installing or
# restarting anything if the known broker has no matching package metadata.
# Do not infer this layout for other distributions or vendor backports.
portal_package_version=''
if command -v dpkg-query >/dev/null 2>&1; then
    portal_package_version=$(dpkg-query -W -f='${db:Status-Status} ${Version}' xdg-desktop-portal 2>/dev/null) ||
        portal_package_version=''
fi
noble_portal_update='^installed 1\.18\.4-1ubuntu2\.24\.04\.[0-9]+$'
if [[ $portal_package_version =~ $noble_portal_update ]] &&
   ! { [[ -f $NOBLE_PORTAL_METADATA ]] && cmp -s -- "$PORTAL_METADATA" "$NOBLE_PORTAL_METADATA"; }; then
    cat >&2 <<EOF
[install-portal] The installed Ubuntu 24.04 broker update (${portal_package_version#installed }) does not discover the per-user jwm.portal.
[install-portal] No files were installed and no services were restarted.
[install-portal] Install JWM metadata through a distro package at:
  $NOBLE_PORTAL_METADATA
[install-portal] Use portal/dist/jwm.portal from this JWM checkout, then rerun this installer.
[install-portal] See packaging/README.md for package staging. Do not set XDG_DESKTOP_PORTAL_DIR.
EOF
    exit 1
fi


# Resolve complete development metadata before Cargo starts any dependency
# build scripts. A shell that only has the PipeWire runtime gets a reusable
# private development prefix, including rpath for D-Bus activation.
PIPEWIRE_ENV=$(bash "$ROOT/scripts/ensure_pipewire.sh" --env)
while IFS= read -r setting; do
    case "$setting" in
        PKG_CONFIG_PATH=*|JWM_PIPEWIRE_PREFIX=*) export "${setting?}" ;;
        '') ;;
        *) echo "[install-portal] unexpected PipeWire environment entry" >&2; exit 1 ;;
    esac
done <<< "$PIPEWIRE_ENV"

echo "[install-portal] building jwm-portal (release)…"
(
    cd "$ROOT"
    cargo build --locked --release --target-dir "$PORTAL_TARGET_DIR" --manifest-path "$PORTAL_MANIFEST"
)

mkdir -p "$LIBEXEC" "$DBUS_SVC" "$PORTAL_DIR" "$PORTAL_CONF"

install -m755 "$PORTAL_BINARY" "$LIBEXEC/jwm-portal"
install -m644 "$PORTAL_METADATA"                    "$PORTAL_DIR/jwm.portal"
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

Verify from a JWM Wayland session (frontend interface check, not a capture test):
  gdbus introspect --session \\
    --dest org.freedesktop.portal.Desktop \\
    --object-path /org/freedesktop/portal/desktop | grep -F org.freedesktop.portal.ScreenCast

A direct call to the JWM backend does not prove frontend routing. Check
ScreenCast in your application in that session. Setting XDG_CURRENT_DESKTOP
on the application alone does not reconfigure an already running broker.

Source consent:
  The portal auto-detects rofi, then wofi. Without a usable picker it cancels.
  JWM_PORTAL_PICKER=rofi|wofi|/path/to/custom-picker overrides auto-detection.
  Configure these in the portal service activation environment, not just the app.

Explicit source overrides for unattended use (unmatched sources cancel):
  JWM_PORTAL_OUTPUT=<output-name-or-substring>
  JWM_PORTAL_WINDOW=class:<app_id>      # or title:<substring>
EOF
