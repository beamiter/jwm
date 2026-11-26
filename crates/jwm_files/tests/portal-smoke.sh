#!/usr/bin/env bash
# Run under the workflow's private Xvfb display. Never reuse the desktop bus.
set -euo pipefail
binary=$1
broker=$2
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(mktemp -d)
trap 'rm -rf -- "$root"' EXIT
cc -std=c11 -Wall -Wextra -Werror -O2 "$here/portal-open-directory.c" \
  -o "$root/open-directory" $(pkg-config --cflags --libs gio-unix-2.0)

export HOME="$root/home"
export XDG_DATA_HOME="$root/data" XDG_CONFIG_HOME="$root/config"
export XDG_CACHE_HOME="$root/cache" XDG_RUNTIME_DIR="$root/run"
export XDG_CURRENT_DESKTOP=JwmFileWorkspaceTest GDK_BACKEND=x11 GSK_RENDERER=cairo
mkdir -p "$HOME" "$XDG_DATA_HOME/applications" "$XDG_CONFIG_HOME/xdg-desktop-portal"
mkdir -m 0700 "$XDG_RUNTIME_DIR"
cat > "$XDG_CONFIG_HOME/xdg-desktop-portal/portals.conf" <<'CONF'
[preferred]
default=gtk
CONF

# A fallback that merely opens the parent does not meet the test.
cat > "$root/fallback.py" <<'PYTHON'
import pathlib
import sys
pathlib.Path(sys.argv[1]).write_text("portal fell back instead of revealing an item\n")
PYTHON
cat > "$XDG_DATA_HOME/applications/org.jwm.PortalNegative.desktop" <<DESKTOP
[Desktop Entry]
Name=Private portal fallback sentinel
Type=Application
Exec=/usr/bin/python3 $root/fallback.py $root/fallback.receipt %f
MimeType=inode/directory;
NoDisplay=true
DESKTOP
cat > "$XDG_CONFIG_HOME/mimeapps.list" <<'CONF'
[Default Applications]
inode/directory=org.jwm.PortalNegative.desktop;
CONF
update-desktop-database "$XDG_DATA_HOME/applications"

timeout 45s dbus-run-session -- bash -s -- "$root" "$binary" "$broker" <<'SESSION'
set -euo pipefail
root=$1 binary=$2 broker=$3
service= portal=
cleanup() {
  local code=$?
  if (( code != 0 )); then
    tail -n 60 "$root/portal.log" "$root/files.log" >&2 || true
  fi
  for pid in "$service" "$portal"; do
    if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; fi
  done
}
trap cleanup EXIT
mkdir -p "$root/start" "$root/portal hidden" "$root/portal regular" "$root/paste-hidden" "$root/paste-regular"
"$broker" --verbose > "$root/portal.log" 2>&1 & portal=$!
"$binary" --file-manager-service -- "$root/start" > "$root/files.log" 2>&1 & service=$!
owner_pid() {
  gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.GetConnectionUnixProcessID "$1" 2>/dev/null || true
}
interfaces_ready() {
  [[ $(owner_pid org.freedesktop.portal.Desktop) == "(uint32 $portal,)" &&
     $(owner_pid org.freedesktop.FileManager1) == "(uint32 $service,)" ]] || return 1
  local portal_version files_api
  portal_version=$(timeout 2s gdbus call --session --dest org.freedesktop.portal.Desktop \
    --object-path /org/freedesktop/portal/desktop --method org.freedesktop.DBus.Properties.Get \
    org.freedesktop.portal.OpenURI version 2>/dev/null) || return 1
  files_api=$(timeout 2s gdbus introspect --session --dest org.freedesktop.FileManager1 \
    --object-path /org/freedesktop/FileManager1 2>/dev/null) || return 1
  [[ "$portal_version" =~ ^\(\<uint32\ [0-9]+\>\,\)$ && "$files_api" == *ShowItems* ]]
}
for i in {1..60}; do
  if interfaces_ready; then break; fi
  sleep 0.1
done
interfaces_ready
wait_window() {
  timeout 8s xdotool search --sync --onlyvisible --name "$1" | sed -n '1p'
}
reveal_and_prove() {
  local tag=$1 name=$2
  local fixture="$root/portal $tag" destination="$root/paste-$tag" file win
  file="$fixture/$name"
  printf 'exact portal item: %s\n' "$tag" > "$file"
  for i in {1..100}; do
    printf 'not the requested item\n' > "$fixture/a-$i.txt"
  done
  "$root/open-directory" "$file"
  win=$(wait_window "^JWM Files · .*/portal $tag\$")
  [[ $(xdotool getwindowpid "$win") == "$service" ]]
  xdotool windowfocus --sync "$win"
  xdotool key --clearmodifiers ctrl+c ctrl+l
  xdotool type --clearmodifiers "$destination"
  xdotool key --clearmodifiers Return
  [[ $(wait_window "^JWM Files · .*/paste-$tag\$") == "$win" ]]
  xdotool key --clearmodifiers ctrl+v
  for i in {1..80}; do
    [[ ! -f "$destination/$name" ]] || break
    sleep 0.1
  done
  cmp "$file" "$destination/$name"
  [[ $(find "$destination" -maxdepth 1 -type f -printf '.' | wc -c) == 1 ]]
  [[ ! -e "$root/fallback.receipt" ]]
}
reveal_and_prove regular 'zz sent through FD % 你好.txt'
echo 'GTK4 X11 portal OpenDirectory: Unix FD selected the exact file without fallback'
reveal_and_prove hidden '.hidden sent through FD % 你好.txt'
echo 'GTK4 X11 portal OpenDirectory: hidden Unix FD selected the exact hidden file without fallback'
SESSION
