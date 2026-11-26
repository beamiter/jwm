#!/usr/bin/env bash
# A real desktop terminal on a private display, with only synthetic folders.
set -euo pipefail
binary=$1
if [[ ${2:-} != --private-xvfb ]]; then
  exec xvfb-run -a -s '-screen 0 1280x800x24 -nolisten tcp -noreset' \
    bash "$0" "$binary" --private-xvfb
fi
command -v xdg-terminal-exec >/dev/null
test -x /usr/bin/xterm
root=$(mktemp -d)
files=
cleanup() {
  local code=$?
  if (( code != 0 )); then
    tail -n 60 "$root/files.log" "$root/adapter.log" 2>/dev/null || true
  fi
  if [[ -n "$files" ]]; then
    kill "$files" 2>/dev/null || true
    wait "$files" 2>/dev/null || true
  fi
  rm -rf -- "$root"
  exit "$code"
}
trap cleanup EXIT
export HOME="$root/home" XDG_DATA_HOME="$root/data" XDG_CONFIG_HOME="$root/config" XDG_CACHE_HOME="$root/cache"
export GDK_BACKEND=x11 GSK_RENDERER=cairo GTK_A11Y=none XDG_CURRENT_DESKTOP=JwmFilesTest
export JWM_FILES_TERMINAL_FIXTURE="$root"
export JWM_FILES_TERMINAL_CHOSEN="$root/"$'folder-%20-\x80'
export SHELL="$root/expected-shell"
mkdir -p "$HOME" "$XDG_DATA_HOME/applications" "$XDG_DATA_HOME/xdg-terminals" "$XDG_CONFIG_HOME" "$JWM_FILES_TERMINAL_CHOSEN"
cat > "$root/expected-shell" <<'SCRIPT'
#!/bin/sh
set -eu
test -t 0
/usr/bin/stat -c '%d:%i' . > "$JWM_FILES_TERMINAL_FIXTURE/shell.tmp"
mv -- "$JWM_FILES_TERMINAL_FIXTURE/shell.tmp" "$JWM_FILES_TERMINAL_FIXTURE/shell.receipt"
sleep 0.4
SCRIPT
cat > "$root/terminal-adapter" <<'SCRIPT'
#!/bin/sh
set -eu
base=$JWM_FILES_TERMINAL_FIXTURE
exec 2>"$base/adapter.log"
# Change the path only after Files chose and called the terminal launcher.
mv -- "$JWM_FILES_TERMINAL_CHOSEN" "$base/held-original"
mkdir -- "$JWM_FILES_TERMINAL_CHOSEN"
printf 'native launcher invoked\n' > "$base/launcher.receipt"
test "$1" = -e
shift
exec /usr/bin/xterm -T 'JWM Files Test Terminal' -e /usr/bin/env "SHELL=$base/expected-shell" "$@"
SCRIPT
chmod 0700 "$root/expected-shell" "$root/terminal-adapter"
cat > "$XDG_DATA_HOME/applications/org.jwm.FilesTerminalTest.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=JWM Files private native terminal
Exec=$root/terminal-adapter
TryExec=$root/terminal-adapter
Categories=System;TerminalEmulator;
Terminal=false
X-TerminalArgExec=-e
X-ExecArg=-e
DESKTOP
# Older packaged xdg-terminal-exec reads xdg-terminals; current versions use applications.
cp "$XDG_DATA_HOME/applications/org.jwm.FilesTerminalTest.desktop" \
  "$XDG_DATA_HOME/xdg-terminals/org.jwm.FilesTerminalTest.desktop"
printf '%s\n' org.jwm.FilesTerminalTest.desktop > "$XDG_CONFIG_HOME/xdg-terminals.list"
expected=$(stat -c '%d:%i' "$JWM_FILES_TERMINAL_CHOSEN")
mkdir "$root/start"
"$binary" -- "$root/start" >"$root/files.log" 2>&1 & files=$!
id=$(timeout 10s xdotool search --sync --onlyvisible --pid "$files" | sed -n '1p')
[[ $(xdotool getwindowpid "$id") == "$files" ]]
printf 'Files fixture window before navigation: %s\n' "$(xdotool getwindowname "$id")"
xdotool windowfocus --sync "$id"
uri=$(python3 -c 'import os,pathlib; print(pathlib.Path(os.environ["JWM_FILES_TERMINAL_CHOSEN"]).as_uri())')
xdotool key --clearmodifiers ctrl+l
xdotool type --clearmodifiers "$uri"
xdotool key --clearmodifiers Return
title=''
for _ in {1..75}; do
  title=$(xdotool getwindowname "$id")
  [[ "$title" != "JWM Files · $root/folder-%20-"* ]] || break
  sleep 0.1
done
printf 'Files fixture window after native URI navigation: %s\n' "$title"
[[ "$title" == "JWM Files · $root/folder-%20-"* ]]
xdotool key --clearmodifiers ctrl+shift+Return
for _ in {1..85}; do [[ ! -f "$root/shell.receipt" ]] || break; sleep 0.1; done
grep -Fx 'native launcher invoked' "$root/launcher.receipt"
[[ $(cat "$root/shell.receipt") == "$expected" ]]
[[ $(stat -c '%d:%i' "$root/held-original") == "$expected" ]]
[[ $(stat -c '%d:%i' "$JWM_FILES_TERMINAL_CHOSEN") != "$expected" ]]
echo 'GTK4 X11 terminal: packaged XDG launcher and actual PTY shell entered the held directory'
