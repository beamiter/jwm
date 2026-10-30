#!/usr/bin/env bash
# Select the patched WebKitGTK and its matching JSC/helper dependencies for one app.
set -euo pipefail
prefix=${WEBKIT_INSTALL_PREFIX:-"$HOME/.local/opt/jwm-webkitgtk"}
if [[ ! -d "$prefix" && ! -v WEBKIT_INSTALL_PREFIX ]]; then
  prefix="$HOME/.local/opt/jwm-webkitgtk-2.50.4"
fi
if [[ $# == 0 ]]; then
  echo "Usage: $0 PROGRAM [ARGS...]" >&2
  exit 2
fi
if [[ ! -f "$prefix/lib/libwebkit2gtk-4.1.so.0" || ! -f "$prefix/lib/libjavascriptcoregtk-4.1.so.0" ]]; then
  echo "Patched WebKitGTK is missing from $prefix; run scripts/build-webkit-transparent.sh first." >&2
  exit 1
fi
prefix=$(cd -- "$prefix" && pwd -P)
export LD_LIBRARY_PATH="$prefix/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

# Distribution packages have absolute helper paths and cannot be relocated
# with LD_LIBRARY_PATH alone. Give just this program the matching helper tree
# at that path; its WebKit sandbox remains enabled. Source builds already
# embed their private prefix and need no mount namespace.
shopt -s nullglob
helper_dirs=("$prefix"/usr/lib/*/webkit2gtk-4.1)
shopt -u nullglob
if ((${#helper_dirs[@]})); then
  if ((${#helper_dirs[@]} != 1)) || ! command -v bwrap >/dev/null; then
    echo "A relocated WebKitGTK package requires one helper directory and bubblewrap." >&2
    exit 1
  fi
  helpers=${helper_dirs[0]}
  for process in WebKitWebProcess WebKitNetworkProcess; do
    if [[ ! -x "$helpers/$process" ]]; then
      echo "Matching WebKitGTK helper is missing: $helpers/$process" >&2
      exit 1
    fi
  done
  exec bwrap --die-with-parent --dev-bind / / \
    --ro-bind "$helpers" "${helpers#"$prefix"}" -- "$@"
fi
exec "$@"
