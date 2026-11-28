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

mkdir -m 700 "$fixture/run" "$fixture/config" "$fixture/data" "$fixture/files" "$fixture/state"
export XDG_STATE_HOME="$fixture/state"
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
grep -F 'GTK4 Wayland mounts: lazy native locations panel opened without changing the folder' "$client_log"
grep -F 'GTK4 Wayland comparison: explicit right-pane copy refused overwrite and refreshed exact bytes' "$client_log"
grep -F 'GTK4 Wayland comparison: replaced right directory required an explicit refresh' "$client_log"
grep -F 'GTK4 Wayland comparison: independent navigation and read-only shortcut focus verified' "$client_log"
grep -F 'GTK4 Wayland windows: cross-window copy preserved conflicts and published exact bytes' "$client_log"
grep -F 'GTK4 Wayland windows: secondary navigation left the original browser unchanged' "$client_log"
grep -F 'GTK4 Wayland grid: list and icon modes kept the same selection' "$client_log"
grep -F 'GTK4 Wayland grid: moving the active directory cleared stale selection' "$client_log"
grep -F 'GTK4 Wayland preview: selected file text shown in the quick preview' "$client_log"
grep -F 'GTK4 Wayland wake: cancelling a dialog delivered deferred invalidation' "$client_log"
grep -F 'GTK4 Wayland copy: delivered conflict status before a successful paste' "$client_log"
grep -F 'GTK4 Wayland copy: keep-both preserved the original and dangling link' "$client_log"
grep -F 'GTK4 Wayland tabs: separate history, cancelled scans and tab buttons verified' "$client_log"
grep -F 'GTK4 Wayland tabs: matching folder names disambiguated and accessible selection changed' "$client_log"
grep -F 'GTK4 Wayland preview: an open preview followed the newly selected same-name file' "$client_log"
grep -F 'GTK4 Wayland undo: directory-bound rename conflicts were non-destructive' "$client_log"
grep -F 'GTK4 Wayland Markdown: native heading style and exact source toggle verified' "$client_log"
grep -F 'GTK4 Wayland PNG: native texture appeared and cleared when returning to Markdown' "$client_log"
grep -F 'GTK4 Wayland JPEG: native texture honored the selected photo orientation' "$client_log"
grep -Fx 'GTK4 Wayland move to Trash: default Enter action cancelled without any write' "$client_log"
grep -Fx 'GTK4 Wayland move to Trash: read and cancel unchanged, fresh write and exact readback' "$client_log"
grep -Fx 'GTK4 Wayland Trash confirmation: stale Copy disposal kept Restore guarded' "$client_log"
grep -Fx 'GTK4 Wayland restore original: default Enter cancelled with payload and sidecar unchanged' "$client_log"
grep -Fx 'GTK4 Wayland restore original: exact inode restored and sibling revision retained' "$client_log"
grep -F 'GTK4 Wayland Trash: same-path entries remained distinct in the home view' "$client_log"
grep -F 'GTK4 Wayland Trash: selected the exact revision and kept both entries on recovery' "$client_log"
grep -F 'GTK4 Wayland Trash pages: next page reached the exact omitted newest entry' "$client_log"
grep -F 'GTK4 Wayland reveal: exact native item selected and scrolled into view' "$client_log"
grep -F 'GTK4 Wayland clipboard: raw filename bytes round-tripped through a file URI' "$client_log"
grep -F 'GTK4 Wayland properties: dangling link metadata and exact raw path shown without follow' "$client_log"
grep -F 'GTK4 Wayland location: raw bytes round-tripped through the editable URI' "$client_log"
grep -Fx 'GTK4 Wayland scoped filename search: local Unicode result, no external or hidden results' "$client_log"
grep -Fx 'GTK4 Wayland scoped search copy: Unicode and exact raw URI, stale rows rejected' "$client_log"
grep -Fx 'GTK4 Wayland batch copy: native picker, exact path bytes, conflict kept, per-file result' "$client_log"
grep -Fx 'GTK4 Wayland folder copy: raw leaf and empty directory, descendant and conflict refused' "$client_log"
grep -Fx 'GTK4 Wayland cut focus: a focused sidebar could neither arm nor consume a cut' "$client_log"
grep -Fx 'GTK4 Wayland cut and paste: native identity retained, no overwrite, cancel, no replay' "$client_log"
grep -Fx 'GTK4 Wayland folder cut: descendant and conflict refused, held destination got the exact folder once' "$client_log"
grep -Fx 'GTK4 Wayland copy journal: fresh state left untouched; saved, incomplete and unreadable records shown without destination check' "$client_log"
grep -Fx 'GTK4 Wayland copy journal check: explicit probe only; refresh dismissed the delayed result' "$client_log"
grep -Fx 'GTK4 Wayland managed journal: new numbered record and pending retired cleanup shown separately' "$client_log"
grep -Fx 'GTK4 Wayland managed journal cleanup: cancel preserved evidence; confirm removed only retired metadata' "$client_log"
grep -Fx 'GTK4 Wayland managed journal cleanup: overlapping refresh did not restore stale pending state' "$client_log"
grep -Fx 'GTK4 Wayland managed journal retirement: cancel, stale selection and exact live metadata move' "$client_log"
grep -F 'GTK4 Wayland find: exact selected row and stale snapshot rejection verified' "$client_log"
grep -F 'GTK4 Wayland find: one early Enter applied once and an edited query discarded it' "$client_log"
