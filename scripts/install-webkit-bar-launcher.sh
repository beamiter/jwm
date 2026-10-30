#!/usr/bin/env bash
# Keep a private WebKit runtime selected when installing/updating a Tauri bar.
set -euo pipefail
if (($# != 2)); then
  echo "Usage: $0 SOURCE_BINARY DESTINATION" >&2
  exit 2
fi
source_binary=$1
destination=$2
prefix=${WEBKIT_INSTALL_PREFIX:-"$HOME/.local/opt/jwm-webkitgtk"}
if [[ ! -d "$prefix" && ! -v WEBKIT_INSTALL_PREFIX ]]; then
  prefix="$HOME/.local/opt/jwm-webkitgtk-2.50.4"
fi
if [[ ! -x "$source_binary" || ! -f "$prefix/lib/libwebkit2gtk-4.1.so.0" ||
      ! -f "$prefix/lib/libjavascriptcoregtk-4.1.so.0" ]]; then
  echo "An executable bar and a complete private WebKitGTK runtime are required." >&2
  exit 1
fi
if head -c 100 "$source_binary" | grep -Fq JWM_PRIVATE_WEBKIT_LAUNCHER; then
  echo "Use the real bar executable as SOURCE_BINARY, not an existing launcher." >&2
  exit 2
fi
bar=$(basename -- "$destination")
case "$bar" in
  tauri_leptos_bar|tauri_react_bar|tauri_solid_bar|tauri_svelte_bar|tauri_vue_bar|tauri_yew_bar) ;;
  *) echo "Not a supported Tauri bar: $bar" >&2; exit 2 ;;
esac
script_dir=$(cd -- "$(dirname -- "$0")" && pwd)
mkdir -p -- "$prefix/bin" "$prefix/libexec/jwm" "$(dirname -- "$destination")"
real_binary="$prefix/libexec/jwm/$bar"
binary_tmp=$(mktemp "$prefix/libexec/jwm/.$bar.XXXXXXXX")
launcher_tmp=$(mktemp "$(dirname -- "$destination")/.$bar.XXXXXXXX")
trap 'rm -f -- "$binary_tmp" "$launcher_tmp"' EXIT
# Rename rather than overwrite an executable that may still be running.
install -m755 -- "$source_binary" "$binary_tmp"
install -m755 -- "$script_dir/run-with-webkit-transparent.sh" "$prefix/bin/run-with-webkit-transparent.sh"
{
  printf '#!/usr/bin/env bash\n# JWM_PRIVATE_WEBKIT_LAUNCHER\n'
  printf 'export WEBKIT_INSTALL_PREFIX=%q\n' "$prefix"
  printf 'exec %q %q "$@"\n' "$prefix/bin/run-with-webkit-transparent.sh" "$real_binary"
} > "$launcher_tmp"
chmod 755 "$launcher_tmp"
if [[ -f "$destination" && ! -e "$destination.before-webkit" ]] &&
   ! head -c 100 "$destination" | grep -Fq JWM_PRIVATE_WEBKIT_LAUNCHER; then
  cp -p -- "$destination" "$destination.before-webkit"
fi
mv -f -- "$binary_tmp" "$real_binary"
mv -f -- "$launcher_tmp" "$destination"
