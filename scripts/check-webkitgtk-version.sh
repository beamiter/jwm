#!/usr/bin/env bash
# Shared Linux build requirement for every tauri_*_bar frontend.
set -euo pipefail
minimum=2.52.6
pkg_config=${PKG_CONFIG:-pkg-config}
if command -v "$pkg_config" >/dev/null &&
   "$pkg_config" --atleast-version="$minimum" webkit2gtk-4.1 2>/dev/null; then
  exit 0
fi
detected=$("$pkg_config" --modversion webkit2gtk-4.1 2>/dev/null) || detected=unavailable
printf 'WebKitGTK >= %s is required to build tauri_*_bar on Linux; detected: %s.\n' "$minimum" "$detected" >&2
printf 'Install/upgrade libwebkit2gtk-4.1-dev and matching JavaScriptCore development packages.\n' >&2
printf 'For a private SDK, set PKG_CONFIG_PATH to its pkgconfig directory. See vendor/webkitgtk/README.md.\n' >&2
exit 1
