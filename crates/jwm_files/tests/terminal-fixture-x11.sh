#!/usr/bin/env bash
# Source only from the existing private real X11 browser fixture.
prepare_terminal_fixture() {
  unset JWM_TEST_TERMINAL_PROBE
  test -x /usr/bin/xterm
  command -v xdg-terminal-exec >/dev/null
  export JWM_TEST_TERMINAL_ROOT="$root/terminal-x11"
  export JWM_TEST_TERMINAL_FOLDER="$JWM_TEST_TERMINAL_ROOT/"$'held-%20-\x80'
  mkdir -p "$JWM_TEST_TERMINAL_FOLDER" "$XDG_DATA_HOME/xdg-terminals"

  cat > "$JWM_TEST_TERMINAL_ROOT/expected-shell" <<'SESSION_SHELL'
#!/bin/sh
set -eu
test -t 0
/usr/bin/stat -c '%d:%i' . > "$JWM_TEST_TERMINAL_ROOT/shell.tmp"
mv -- "$JWM_TEST_TERMINAL_ROOT/shell.tmp" "$JWM_TEST_TERMINAL_ROOT/shell.receipt"
sleep 0.4
SESSION_SHELL
  cat > "$JWM_TEST_TERMINAL_ROOT/launcher" <<'SESSION_LAUNCHER'
#!/bin/sh
set -eu
root=$JWM_TEST_TERMINAL_ROOT
if [ "${JWM_TEST_TERMINAL_PROBE:-}" = 1 ]; then
  test "$#" = 2
  test "$1" = -e
  test "$2" = /bin/true
  printf '%s\n' 'private terminal descriptor selected' > "$root/lookup.receipt"
  exit 0
fi
exec 2>"$root/xterm.log"
printf '%s\n' 'packaged selector invoked' > "$root/selector.receipt"
test "$1" = -e
shift
# Only mutate the private synthetic path after the product pinned it.
mv -- "$JWM_TEST_TERMINAL_FOLDER" "$root/original"
mkdir -- "$JWM_TEST_TERMINAL_FOLDER"
exec /usr/bin/xterm -fn fixed -T 'JWM Files Terminal Fixture' -e \
  /usr/bin/env "SHELL=$root/expected-shell" "$@"
SESSION_LAUNCHER
  chmod 0700 "$JWM_TEST_TERMINAL_ROOT/expected-shell" "$JWM_TEST_TERMINAL_ROOT/launcher"
  local descriptor=org.jwm.FilesTerminalFixture.desktop
  cat > "$XDG_DATA_HOME/applications/$descriptor" <<DESKTOP
[Desktop Entry]
Type=Application
Name=JWM Files native terminal fixture
Exec=$JWM_TEST_TERMINAL_ROOT/launcher
TryExec=$JWM_TEST_TERMINAL_ROOT/launcher
Categories=System;TerminalEmulator;
Terminal=false
X-TerminalArgExec=-e
X-ExecArg=-e
DESKTOP
  # Ubuntu's older selector uses xdg-terminals, newer releases applications.
  cp "$XDG_DATA_HOME/applications/$descriptor" "$XDG_DATA_HOME/xdg-terminals/$descriptor"
  printf '%s\n' "$descriptor" > "$XDG_CONFIG_HOME/xdg-terminals.list"
  JWM_TEST_TERMINAL_PROBE=1 timeout 5s xdg-terminal-exec /bin/true
  grep -Fx 'private terminal descriptor selected' "$JWM_TEST_TERMINAL_ROOT/lookup.receipt"
}

exercise_terminal_fixture() {
  local dest="$JWM_TEST_TERMINAL_ROOT" expected uri
  expected=$(stat -c '%d:%i' "$JWM_TEST_TERMINAL_FOLDER")
  # The caller already proved this exact Files window is mapped and navigable.
  xdotool windowfocus --sync "$window"
  uri=$(python3 -c 'import os,pathlib; print(pathlib.Path(os.environ["JWM_TEST_TERMINAL_FOLDER"]).as_uri())')
  xdotool key --clearmodifiers ctrl+l ctrl+a BackSpace
  xdotool type --clearmodifiers "$uri"
  xdotool key --clearmodifiers Return
  [[ $(title_id '^JWM Files · .*/terminal-x11/held-%20-.*$') == "$window" ]]
  printf '%s\n' 'GTK4 X11 terminal: known native Files window selected the raw path'
  xdotool key --clearmodifiers ctrl+shift+Return
  for _ in {1..95}; do
    [[ ! -f "$dest/shell.receipt" ]] || break
    sleep 0.1
  done
  if [[ ! -f "$dest/shell.receipt" ]]; then
    cat "$dest/selector.receipt" "$dest/xterm.log" >&2 || true
    echo 'terminal PTY did not report final cwd' >&2
    return 1
  fi
  grep -Fx 'packaged selector invoked' "$dest/selector.receipt"
  [[ $(cat "$dest/shell.receipt") == "$expected" ]]
  [[ $(stat -c '%d:%i' "$dest/original") == "$expected" ]]
  [[ $(stat -c '%d:%i' "$JWM_TEST_TERMINAL_FOLDER") != "$expected" ]]
  printf '%s\n' 'GTK4 X11 terminal: packaged XDG launcher and actual PTY shell entered the held directory'
}
