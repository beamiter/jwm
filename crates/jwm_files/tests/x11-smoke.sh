#!/usr/bin/env bash
set -euo pipefail

binary=$1
root=$(mktemp -d)
child=
cleanup() {
  if [[ -n "$child" ]]; then
    kill "$child" 2>/dev/null || true
    wait "$child" 2>/dev/null || true
  fi
  rm -rf -- "$root"
}
trap cleanup EXIT

mkdir -p "$root/start" "$root/next"
printf 'test note\n' > "$root/next/test.txt"

export XDG_DATA_HOME="$root/xdg-data" XDG_CONFIG_HOME="$root/xdg-config"
mkdir -p "$XDG_DATA_HOME/applications" "$XDG_CONFIG_HOME"
mkdir -p "$root/open-doc" "$root/open-link" "$root/open-broken" "$root/link-target"
printf 'original\n' > "$root/open-doc/"$'report space%\x80.txt'
printf 'lookalike\n' > "$root/open-doc/"$'report space%\xef\xbf\xbd.txt'
ln -s ../link-target "$root/open-link/linked"
ln -s ../missing "$root/open-broken/broken"
cat > "$root/record.py" <<'PYTHON'
import os
import sys
with open(os.path.join(os.path.dirname(__file__), "launch.receipt"), "ab") as out:
    out.write(os.fsencode(sys.argv[1]) + b"\n")
PYTHON
cat > "$XDG_DATA_HOME/applications/org.jwm.TestOpen.desktop" <<DESKTOP
[Desktop Entry]
Name=JWM Files private test handler
Type=Application
Exec=/usr/bin/python3 $root/record.py %f
MimeType=text/plain;
NoDisplay=true
DESKTOP
cat > "$XDG_CONFIG_HOME/mimeapps.list" <<'DESKTOP'
[Default Applications]
text/plain=org.jwm.TestOpen.desktop;
DESKTOP


title_id() {
  timeout 15s xdotool search --sync --onlyvisible --name "$1" | sed -n '1p'
}

for attempt in 1 2 3 4 5; do
  if xdotool getdisplaygeometry >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done
xdotool getdisplaygeometry >/dev/null

GDK_BACKEND=x11 GSK_RENDERER=cairo "$binary" "$root/start" &
child=$!
window=$(title_id '^JWM Files · .*/start$')
xdotool windowfocus --sync "$window"
xdotool key --clearmodifiers ctrl+l
xdotool type --clearmodifiers "$root/next"
xdotool key Return
next=$(title_id '^JWM Files · .*/next$')
[[ "$next" == "$window" ]]
printf 'GTK4 X11 smoke: keyboard navigation opened the new directory in the same window\n'

xdotool windowfocus --sync "$window"
xdotool key --clearmodifiers ctrl+shift+n
dialog=$(title_id '^JWM Files · New Folder$')
xdotool windowfocus --sync "$dialog"
xdotool type --clearmodifiers 'smoke-child'
xdotool key Return
for attempt in {1..30}; do
  [[ ! -d "$root/next/smoke-child" ]] || break
  sleep 0.1
done
[[ -d "$root/next/smoke-child" ]]
[[ ! -e "$root/start/smoke-child" ]]
[[ $(cat "$root/next/test.txt") == 'test note' ]]
printf 'GTK4 X11 smoke: New Folder wrote only into the selected directory\n'

xdotool windowfocus --sync "$window"
xdotool key --clearmodifiers alt+Left
back=$(title_id '^JWM Files · .*/start$')
[[ "$back" == "$window" ]]
xdotool key --clearmodifiers alt+Right
forward=$(title_id '^JWM Files · .*/next$')
[[ "$forward" == "$window" ]]
printf 'GTK4 X11 smoke: Back and Forward revisited both directories in the same window\n'

visit_path() {
  xdotool windowfocus --sync "$window"
  xdotool key --clearmodifiers ctrl+l
  xdotool type --clearmodifiers "$root/$1"
  xdotool key Return
  [[ $(title_id "^JWM Files · .*/$1\$") == "$window" ]]
}
activate_first_row() {
  # The view is populated in batches after the new title is set.
  sleep 0.2
  xdotool mousemove --window "$window" 350 72 click --repeat 2 --delay 85 1
}

visit_path open-doc
activate_first_row
for attempt in {1..50}; do
  [[ ! -s "$root/launch.receipt" ]] || break
  sleep 0.1
done
sleep 0.2
printf '%s\n' "$root/open-doc/"$'report space%\x80.txt' > "$root/expected.receipt"
cmp "$root/expected.receipt" "$root/launch.receipt"
printf 'GTK4 X11 smoke: the MIME handler received the exact non-UTF-8 path once\n'

visit_path open-link
activate_first_row
[[ $(title_id '^JWM Files · .*/open-link/linked$') == "$window" ]]

visit_path open-broken
activate_first_row
sleep 0.3
[[ $(xdotool getwindowname "$window") == "JWM Files · $root/open-broken" ]]
cmp "$root/expected.receipt" "$root/launch.receipt"
printf 'GTK4 X11 smoke: directory links stayed in the browser and broken links did not launch\n'

mkdir -p "$root/rename-fixture"
printf 'source\n' > "$root/rename-fixture/a-source.txt"
printf 'destination\n' > "$root/rename-fixture/z-existing.txt"
visit_path rename-fixture
rename_first_row() {
  sleep 0.2
  xdotool mousemove --window "$window" 350 72 click 1
  xdotool key --clearmodifiers F2
  local dialog
  dialog=$(title_id '^JWM Files · Rename$')
  xdotool windowfocus --sync "$dialog"
  xdotool key --clearmodifiers ctrl+a
  xdotool type --clearmodifiers "$1"
  xdotool key Return
}
rename_first_row z-existing.txt
sleep 0.5
[[ $(cat "$root/rename-fixture/a-source.txt") == 'source' ]]
[[ $(cat "$root/rename-fixture/z-existing.txt") == 'destination' ]]
xdotool windowfocus --sync "$window"
rename_first_row renamed.txt
for attempt in {1..30}; do
  [[ ! -f "$root/rename-fixture/renamed.txt" ]] || break
  sleep 0.1
done
[[ ! -e "$root/rename-fixture/a-source.txt" ]]
[[ $(cat "$root/rename-fixture/renamed.txt") == 'source' ]]
[[ $(cat "$root/rename-fixture/z-existing.txt") == 'destination' ]]
printf 'GTK4 X11 smoke: F2 renamed the selected item without replacing a conflict\n'
