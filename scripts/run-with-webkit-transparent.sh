#!/usr/bin/env bash
# Select the patched WebKitGTK and its matching JSC/helper dependencies for one app.
set -euo pipefail
prefix=${WEBKIT_INSTALL_PREFIX:-"$HOME/.local/opt/jwm-webkitgtk-2.50.4"}
if [[ $# == 0 ]]; then
  echo "Usage: $0 PROGRAM [ARGS...]" >&2
  exit 2
fi
if [[ ! -f "$prefix/lib/libwebkit2gtk-4.1.so.0" || ! -f "$prefix/lib/libjavascriptcoregtk-4.1.so.0" ]]; then
  echo "Patched WebKitGTK is missing from $prefix; run scripts/build-webkit-transparent.sh first." >&2
  exit 1
fi
export LD_LIBRARY_PATH="$prefix/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$@"
