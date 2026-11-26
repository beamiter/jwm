#!/usr/bin/env bash
set -euo pipefail

fixture=$(mktemp -d)
weston_pid=
client_log="$fixture/client.log"
weston_log="$fixture/weston.log"
cleanup() {
  local result=$?
  if [[ -n "$weston_pid" ]]; then
    kill "$weston_pid" 2>/dev/null || true
    wait "$weston_pid" 2>/dev/null || true
  fi
  if [[ "$result" != 0 ]]; then
    if [[ -f "$weston_log" ]]; then tail -n 80 "$weston_log" >&2; fi
    if [[ -f "$client_log" ]]; then tail -n 120 "$client_log" >&2; fi
  fi
  rm -rf -- "$fixture"
}
trap cleanup EXIT

mkdir -m 700 "$fixture/run" "$fixture/config" "$fixture/data" "$fixture/files"
export XDG_RUNTIME_DIR="$fixture/run" XDG_CONFIG_HOME="$fixture/config" XDG_DATA_HOME="$fixture/data"
export WAYLAND_DISPLAY=jwm-files-test GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=test
export JWM_FILE_SMOKE_FIXTURE="$fixture/files"
unset DISPLAY WAYLAND_SOCKET
weston --backend=headless --renderer=pixman --shell=kiosk --no-config   --socket="$WAYLAND_DISPLAY" --width=1280 --height=800 --idle-time=0   --log="$weston_log" &
weston_pid=$!
for _ in {1..80}; do
  [[ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]] || break
  if ! kill -0 "$weston_pid" 2>/dev/null; then
    echo 'Weston exited before its private socket became available' >&2
    exit 1
  fi
  sleep 0.1
done
[[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]
WAYLAND_DEBUG=client timeout 45s cargo test --locked native_wayland_window_maps_and_navigates --   --ignored --nocapture >"$client_log" 2>&1
grep -F 'GTK4 Wayland smoke: native window mapped and navigation replaced the listing' "$client_log"
grep -Eq 'xdg_surface[@#][0-9]+\.configure\(' "$client_log"
grep -Eq 'wl_surface[@#][0-9]+\.attach\(wl_buffer' "$client_log"
printf 'GTK4 Wayland smoke: compositor configured a native surface and the client attached a buffer\n'
grep -F 'GTK4 Wayland grid: list and icon modes kept the same selection' "$client_log"
grep -F 'GTK4 Wayland grid: moving the active directory cleared stale selection' "$client_log"
grep -F 'GTK4 Wayland preview: selected file text shown in the quick preview' "$client_log"
grep -F 'GTK4 Wayland wake: cancelling a dialog delivered deferred invalidation' "$client_log"
grep -F 'GTK4 Wayland copy: delivered conflict status before a successful paste' "$client_log"
grep -F 'GTK4 Wayland tabs: separate history, cancelled scans and tab buttons verified' "$client_log"
grep -F 'GTK4 Wayland tabs: matching folder names disambiguated and accessible selection changed' "$client_log"
grep -F 'GTK4 Wayland preview: an open preview followed the newly selected same-name file' "$client_log"
grep -F 'GTK4 Wayland undo: directory-bound rename conflicts were non-destructive' "$client_log"
