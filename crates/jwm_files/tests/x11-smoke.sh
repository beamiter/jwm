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
