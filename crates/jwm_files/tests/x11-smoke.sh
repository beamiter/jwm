#!/usr/bin/env bash
set -euo pipefail

binary=$1
root=$(mktemp -d)
child=
desktop_child=
cleanup() {
  if [[ -n "$desktop_child" && $(readlink "/proc/$desktop_child/exe" || true) == "$binary" ]]; then
    kill "$desktop_child" 2>/dev/null || true
  fi
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
MimeType=text/plain;inode/directory;
NoDisplay=true
DESKTOP
cat > "$XDG_CONFIG_HOME/mimeapps.list" <<'DESKTOP'
[Default Applications]
text/plain=org.jwm.TestOpen.desktop;
inode/directory=org.jwm.TestOpen.desktop;
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
  # Wait for the first row widgets to bind following navigation.
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

# Revisit the directory through a fresh listing so the successful rename
# receipt is already available before delivering a real undo shortcut.
visit_path next
visit_path rename-fixture
xdotool key --clearmodifiers ctrl+l
xdotool type --clearmodifiers 'unsubmitted-text'
xdotool key --clearmodifiers ctrl+z Escape
[[ ! -e "$root/rename-fixture/a-source.txt" ]]
[[ $(cat "$root/rename-fixture/renamed.txt") == 'source' ]]
xdotool key --clearmodifiers ctrl+z
for attempt in {1..40}; do
  [[ ! -f "$root/rename-fixture/a-source.txt" ]] || break
  sleep 0.1
done
[[ $(cat "$root/rename-fixture/a-source.txt") == 'source' ]]
[[ ! -e "$root/rename-fixture/renamed.txt" ]]
[[ $(cat "$root/rename-fixture/z-existing.txt") == 'destination' ]]
printf 'GTK4 X11 undo: Ctrl+Z restored the original name without capturing address edits\n'

mkdir -p "$root/watch-fixture"
printf 'first\n' > "$root/watch-fixture/a-first.txt"
printf 'target\n' > "$root/watch-fixture/m-middle.txt"
printf 'last\n' > "$root/watch-fixture/z-last.txt"
visit_path watch-fixture
sleep 0.2
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers Down
printf 'ahead\n' > "$root/watch-fixture/0-ahead.txt"
sleep 0.65
xdotool key --clearmodifiers F2
dialog=$(title_id '^JWM Files · Rename$')
xdotool windowfocus --sync "$dialog"
xdotool key --clearmodifiers ctrl+a
xdotool type --clearmodifiers 'n-kept-selection.txt'
xdotool key Return
for attempt in {1..30}; do
  [[ ! -f "$root/watch-fixture/n-kept-selection.txt" ]] || break
  sleep 0.1
done
[[ $(cat "$root/watch-fixture/n-kept-selection.txt") == 'target' ]]
[[ $(cat "$root/watch-fixture/a-first.txt") == 'first' ]]
[[ $(cat "$root/watch-fixture/0-ahead.txt") == 'ahead' ]]
sleep 0.65
xdotool windowfocus --sync "$window"
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers F2
dialog=$(title_id '^JWM Files · Rename$')
xdotool windowfocus --sync "$dialog"
xdotool key Escape
xdotool windowfocus --sync "$window"
rm "$root/watch-fixture/0-ahead.txt"
sleep 0.65
xdotool key --clearmodifiers F2
sleep 0.15
if xdotool search --onlyvisible --name '^JWM Files · Rename$' >/dev/null 2>&1; then
  echo 'Deletion left a substitute selected' >&2
  exit 1
fi
printf 'GTK4 X11 smoke: watcher refresh kept an unchanged selection and cleared a deleted one\n'

mkdir -p "$root/location-cancel"
xdotool windowfocus --sync "$window"
xdotool key --clearmodifiers ctrl+l
xdotool type --clearmodifiers "$root/location-cancel"
xdotool key Escape
sleep 0.25
[[ $(xdotool getwindowname "$window") == "JWM Files · $root/watch-fixture" ]]
visit_path location-cancel
printf 'GTK4 X11 smoke: Escape cancelled address editing and Ctrl+L reopened it\n'

mkdir -p "$root/grid-switch"
printf 'intended\n' > "$root/grid-switch/a-target.txt"
printf 'untouched\n' > "$root/grid-switch/z-other.txt"
visit_path grid-switch
sleep 0.2
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers ctrl+2 F2
dialog=$(title_id '^JWM Files · Rename$')
xdotool windowfocus --sync "$dialog"
xdotool key --clearmodifiers ctrl+a
xdotool type --clearmodifiers 'grid-renamed.txt'
xdotool key Return
for attempt in {1..30}; do
  [[ ! -f "$root/grid-switch/grid-renamed.txt" ]] || break
  sleep 0.1
done
[[ ! -e "$root/grid-switch/a-target.txt" ]]
[[ $(cat "$root/grid-switch/grid-renamed.txt") == 'intended' ]]
[[ $(cat "$root/grid-switch/z-other.txt") == 'untouched' ]]
xdotool windowfocus --sync "$window"
xdotool key --clearmodifiers ctrl+1
printf 'GTK4 X11 grid: switching modes preserved the selected rename target\n'

mkdir -p "$root/copy-from" "$root/copy-to" "$root/copy-conflict"
copy_name=$'file space%\x80.txt'
printf 'copy me exactly\n' > "$root/copy-from/$copy_name"
printf 'existing\n' > "$root/copy-conflict/$copy_name"
visit_path copy-from
sleep 0.2
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers ctrl+c
visit_path copy-conflict
xdotool key --clearmodifiers ctrl+v
sleep 0.55
[[ $(cat "$root/copy-conflict/$copy_name") == 'existing' ]]
visit_path copy-to
xdotool key --clearmodifiers ctrl+v
for attempt in {1..40}; do
  [[ ! -f "$root/copy-to/$copy_name" ]] || break
  sleep 0.1
done
cmp "$root/copy-from/$copy_name" "$root/copy-to/$copy_name"
[[ $(find "$root/copy-to" -mindepth 1 -maxdepth 1 -type f | wc -l) == 1 ]]
[[ $(cat "$root/copy-conflict/$copy_name") == 'existing' ]]
printf 'GTK4 X11 copy: exact native bytes were copied without overwriting a conflict\n'

xdotool key --clearmodifiers ctrl+l
xdotool type --clearmodifiers "$root/next"
xdotool key --clearmodifiers ctrl+a ctrl+c
xdotool key --clearmodifiers BackSpace ctrl+v
sleep 0.25
xdotool key --clearmodifiers Return
[[ $(title_id '^JWM Files · .*/next$') == "$window" ]]
[[ ! -e "$root/next/$copy_name" ]]
printf 'GTK4 X11 copy: address editing retained normal text clipboard shortcuts\n'

# The clipboard path shortcut must populate an editable field, not the
# process-local file-copy buffer. Navigate only from that pasted text.
mkdir -p "$root/path-copy/target folder %"
visit_path path-copy
sleep 0.2
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers ctrl+shift+c ctrl+l ctrl+a BackSpace ctrl+v
sleep 0.2
xdotool key --clearmodifiers Return
[[ $(title_id '^JWM Files · .*/path-copy/target folder %$') == "$window" ]]
printf 'GTK4 X11 clipboard: Ctrl+Shift+C pasted the exact path into the address bar\n'

# The lossy lookalike has the same rendered name. Prove the reopened
# directory's native identity via a write only to the disposable fixture.
raw_dir="$root/uri-source/"$'a-raw-%20-\x80'
lookalike="$root/uri-source/"$'a-raw-%20-\xef\xbf\xbd'
mkdir -p "$raw_dir" "$lookalike" "$root/uri-away"
visit_path uri-source
sleep 0.2
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers ctrl+shift+c
visit_path uri-away
xdotool key --clearmodifiers ctrl+l ctrl+a BackSpace ctrl+v
sleep 0.2
xdotool key --clearmodifiers Return
[[ $(title_id '^JWM Files · .*/uri-source/a-raw-%20-.*$') == "$window" ]]
xdotool key --clearmodifiers ctrl+shift+n
uri_dialog=$(title_id '^JWM Files · New Folder$')
xdotool windowfocus --sync "$uri_dialog"
xdotool type --clearmodifiers 'raw-uri-proof'
xdotool key --clearmodifiers Return
for attempt in {1..35}; do
  [[ ! -d "$raw_dir/raw-uri-proof" ]] || break
  sleep 0.1
done
[[ -d "$raw_dir/raw-uri-proof" && ! -e "$lookalike/raw-uri-proof" ]]
printf 'GTK4 X11 location: copied raw file URI reopened the byte-exact directory\n'

mkdir -p "$root/uri-away/literal%20" "$root/uri-away/literal "
visit_path uri-away/literal%20
xdotool key --clearmodifiers ctrl+l ctrl+a BackSpace
xdotool type --clearmodifiers "file://outside.invalid$root/uri-away"
xdotool key --clearmodifiers Return
sleep 0.25
[[ $(xdotool getwindowname "$window") == "JWM Files · $root/uri-away/literal%20" ]]
xdotool key --clearmodifiers Escape
printf 'GTK4 X11 location: rejected a remote URI and kept literal percent path semantics\n'

mkdir -p "$root/find-in-folder/zz intended target"
for i in {1..90}; do mkdir "$root/find-in-folder/a-$i"; done
visit_path find-in-folder
xdotool key --clearmodifiers ctrl+f
sleep 0.25
xdotool type --clearmodifiers 'INTENDED TARGET'
# Searching and stepping must not activate the directory until we paste
# the copied selection into the address bar.
for attempt in {1..6}; do
  xdotool key --clearmodifiers Return
  sleep 0.12
done
[[ $(xdotool getwindowname "$window") == "JWM Files · $root/find-in-folder" ]]
xdotool key --clearmodifiers Escape ctrl+shift+c ctrl+l ctrl+a BackSpace ctrl+v
sleep 0.2
xdotool key --clearmodifiers Return
[[ $(title_id '^JWM Files · .*/find-in-folder/zz intended target$') == "$window" ]]
printf 'GTK4 X11 find: Ctrl+F selected the offscreen literal filename without activating it\n'

visit_path copy-from
sleep 0.2
xdotool mousemove --window "$window" 350 72 click 1
xdotool key --clearmodifiers space
preview=$(title_id '^JWM Files · Preview$')
[[ "$preview" != "$window" ]]
xdotool windowfocus --sync "$preview"
xdotool key --clearmodifiers Escape
for attempt in {1..20}; do
  xdotool search --onlyvisible --name '^JWM Files · Preview$' >/dev/null 2>&1 || break
  sleep 0.1
done
if xdotool search --onlyvisible --name '^JWM Files · Preview$' >/dev/null 2>&1; then
  echo 'Quick preview remained visible after Escape' >&2
  exit 1
fi
[[ $(title_id '^JWM Files · .*/copy-from$') == "$window" ]]
printf 'GTK4 X11 preview: Space opened the selected text and Escape returned to browsing\n'

mkdir -p "$root/tab-start" "$root/tab-branch-a" "$root/tab-branch-b"
visit_path tab-start
visit_path tab-branch-a
xdotool key --clearmodifiers ctrl+t
visit_path tab-branch-b
xdotool key --clearmodifiers ctrl+Page_Up
[[ $(title_id '^JWM Files · .*/tab-branch-a$') == "$window" ]]
xdotool key --clearmodifiers alt+Left
[[ $(title_id '^JWM Files · .*/tab-start$') == "$window" ]]
xdotool key --clearmodifiers ctrl+Page_Down
[[ $(title_id '^JWM Files · .*/tab-branch-b$') == "$window" ]]
xdotool key --clearmodifiers alt+Left
[[ $(title_id '^JWM Files · .*/tab-branch-a$') == "$window" ]]
xdotool key --clearmodifiers ctrl+w
[[ $(title_id '^JWM Files · .*/tab-start$') == "$window" ]]
xdotool key --clearmodifiers ctrl+w
# A later navigation proves the last tab is still interactive.
visit_path tab-branch-b
printf 'GTK4 X11 tabs: real shortcuts kept both histories and left the final tab open\n'

xdotool key --clearmodifiers ctrl+shift+t
[[ $(title_id '^JWM Files · .*/tab-branch-a$') == "$window" ]]
xdotool key --clearmodifiers alt+Right
[[ $(title_id '^JWM Files · .*/tab-branch-b$') == "$window" ]]
xdotool key --clearmodifiers alt+Left
[[ $(title_id '^JWM Files · .*/tab-branch-a$') == "$window" ]]
xdotool key --clearmodifiers ctrl+w
[[ $(title_id '^JWM Files · .*/tab-branch-b$') == "$window" ]]
printf 'GTK4 X11 tabs: Ctrl+Shift+T restored the exact closed tab and its forward history\n'

# Launch through the shipped desktop entry in a disposable XDG profile.
desktop_entry="$(dirname "$0")/../data/org.jwm.Files.desktop"
desktop_folder="$root/desktop fixture % 你好"
mkdir "$desktop_folder"
before_mime=$(sha256sum "$XDG_CONFIG_HOME/mimeapps.list")
before_default=$(gio mime inode/directory | sed -n '1p')
[[ "$before_default" == *org.jwm.TestOpen.desktop* ]]
install -m644 "$desktop_entry" "$XDG_DATA_HOME/applications/org.jwm.Files.desktop"
PATH="$(dirname "$binary"):$PATH" GDK_BACKEND=x11 GSK_RENDERER=cairo \
  gio launch "$XDG_DATA_HOME/applications/org.jwm.Files.desktop" "$desktop_folder"
desktop_window=$(title_id '^JWM Files · .*/desktop fixture % 你好$')
[[ -n "$desktop_window" && "$desktop_window" != "$window" ]]
desktop_child=$(xdotool getwindowpid "$desktop_window")
[[ "$desktop_child" =~ ^[0-9]+$ && $(readlink "/proc/$desktop_child/exe") == "$binary" ]]
[[ $(xdotool getwindowname "$desktop_window") == "JWM Files · $desktop_folder" ]]
[[ "$before_mime" == "$(sha256sum "$XDG_CONFIG_HOME/mimeapps.list")" ]]
[[ "$before_default" == "$(gio mime inode/directory | sed -n '1p')" ]]
# In a clean profile where no default exists, the launcher must not become
# a fallback handler after the desktop cache is rebuilt.
! grep -q '^MimeType=' "$desktop_entry"
clean_root="$root/no-folder-handler"
mkdir -p "$clean_root/data/applications" "$clean_root/config" "$clean_root/sys" "$clean_root/etc"
clean_mime() {
  XDG_DATA_HOME="$clean_root/data" XDG_CONFIG_HOME="$clean_root/config" \
    XDG_DATA_DIRS="$clean_root/sys" XDG_CONFIG_DIRS="$clean_root/etc" gio mime inode/directory
}
clean_before=$(clean_mime)
install -m644 "$desktop_entry" "$clean_root/data/applications/org.jwm.Files.desktop"
if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$clean_root/data/applications"
fi
[[ "$clean_before" == "$(clean_mime)" ]]
printf 'GTK4 X11 launcher: menu-only entry preserved an empty folder resolution\n'
printf 'GTK4 X11 launcher: shipped desktop entry opened the exact local folder without changing defaults\n'

# Opt-in service runs only in an isolated session bus, never an existing desktop bus.
GDK_BACKEND=x11 GSK_RENDERER=cairo timeout 35s dbus-run-session -- bash -s -- "$binary" "$root" <<'BUS'
set -euo pipefail
binary=$1
root=$2
first=
second=
ordinary=
cleanup() {
  if [[ -n "$ordinary" ]]; then kill "$ordinary" 2>/dev/null || true; wait "$ordinary" 2>/dev/null || true; fi
  if [[ -n "$second" ]]; then kill "$second" 2>/dev/null || true; wait "$second" 2>/dev/null || true; fi
  if [[ -n "$first" ]]; then kill "$first" 2>/dev/null || true; wait "$first" 2>/dev/null || true; fi
}
trap cleanup EXIT
name=org.freedesktop.FileManager1
export_file() {
  gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
    --method org.freedesktop.DBus.GetNameOwner "$name"
}
if export_file >/dev/null 2>&1; then
  echo 'fresh session bus unexpectedly has a FileManager1 owner' >&2
  exit 1
fi
first_path="$root/bus-start"
second_path="$root/bus-other"
opened_path="$root/bus exact % 你好"
ordinary_path="$root/bus-ordinary"
mkdir "$first_path" "$second_path" "$opened_path" "$ordinary_path"
"$binary" -- "$ordinary_path" > "$root/bus-ordinary.log" 2>&1 & ordinary=$!
ordinary_window=$(timeout 10s xdotool search --sync --onlyvisible --name '^JWM Files · .*/bus-ordinary$' | sed -n '1p')
[[ $(xdotool getwindowpid "$ordinary_window") == "$ordinary" ]]
if export_file >/dev/null 2>&1; then
  echo 'ordinary window must not claim FileManager1' >&2
  exit 1
fi
"$binary" --file-manager-service -- "$first_path" > "$root/bus-first.log" 2>&1 & first=$!
owner=
for n in {1..50}; do
  if owner=$(export_file 2>/dev/null); then break; fi
  sleep 0.1
done
[[ -n "$owner" ]]
"$binary" --file-manager-service -- "$second_path" > "$root/bus-second.log" 2>&1 & second=$!
for n in {1..50}; do
  if grep -Fq 'session name unavailable; existing owner unchanged' "$root/bus-second.log"; then break; fi
  sleep 0.1
done
grep -Fq 'session name unavailable; existing owner unchanged' "$root/bus-second.log"
[[ $(export_file) == "$owner" ]]
uri=$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).as_uri())' "$opened_path")
call() {
  gdbus call --session --dest "$name" --object-path /org/freedesktop/FileManager1 \
    --method org.freedesktop.FileManager1.ShowFolders "$1" ""
}
if call "['$uri','file://remote.invalid/tmp/not-local']" > "$root/bus-invalid.log" 2>&1; then
  echo 'invalid mixed local and remote request was not rejected' >&2
  exit 1
fi
grep -Fq 'org.freedesktop.DBus.Error.InvalidArgs' "$root/bus-invalid.log"
if xdotool search --onlyvisible --name '^JWM Files · .*/bus exact % 你好$' >/dev/null 2>&1; then
  echo 'invalid batch partially opened a folder' >&2
  exit 1
fi
call "['$uri']"
opened=$(timeout 10s xdotool search --sync --onlyvisible --name '^JWM Files · .*/bus exact % 你好$' | sed -n '1p')
[[ $(xdotool getwindowname "$opened") == "JWM Files · $opened_path" ]]
[[ $(xdotool getwindowpid "$opened") == "$first" ]]
printf 'GTK4 X11 FileManager1: opt-in folder request opened exact native folder and preserved existing owner\n'
missing_path="$root/bus-missing"
mkdir "$missing_path"
missing_uri=$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).as_uri())' "$missing_path/does-not-exist")
if gdbus call --session --dest "$name" --object-path /org/freedesktop/FileManager1 \
  --method org.freedesktop.FileManager1.ShowItems "['$missing_uri']" "" > "$root/reveal-missing.log" 2>&1; then
  echo 'ShowItems returned success without selecting an item' >&2
  exit 1
fi
grep -Fq 'The requested native item was not found in its parent folder' "$root/reveal-missing.log"
printf 'GTK4 X11 FileManager1: ShowItems refused to acknowledge a missing item\n'
reveal_path="$root/bus-reveal"
paste_path="$root/bus-copy"
mkdir "$reveal_path" "$paste_path"
reveal_file="$reveal_path/zz exact % 你好.txt"
printf 'selected over native D-Bus\n' > "$reveal_file"
for i in {1..90}; do touch "$reveal_path/a-$i.txt"; done
item_uri=$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).as_uri())' "$reveal_file")
gdbus call --session --dest "$name" --object-path /org/freedesktop/FileManager1 \
  --method org.freedesktop.FileManager1.ShowItems "['$item_uri']" ""
reveal_window=$(timeout 10s xdotool search --sync --onlyvisible --name '^JWM Files · .*/bus-reveal$' | sed -n '1p')
[[ $(xdotool getwindowpid "$reveal_window") == "$first" ]]
xdotool windowfocus --sync "$reveal_window"
xdotool key --clearmodifiers ctrl+c ctrl+l
xdotool type --clearmodifiers "$paste_path"
xdotool key Return
[[ $(timeout 10s xdotool search --sync --onlyvisible --name '^JWM Files · .*/bus-copy$' | sed -n '1p') == "$reveal_window" ]]
xdotool key --clearmodifiers ctrl+v
for n in {1..50}; do
  [[ ! -f "$paste_path/${reveal_file##*/}" ]] || break
  sleep 0.1
done
cmp "$reveal_file" "$paste_path/${reveal_file##*/}"
printf 'GTK4 X11 FileManager1: ShowItems selected the requested file rather than an adjacent row\n'

gdbus call --session --dest "$name" --object-path /org/freedesktop/FileManager1 \
  --method org.freedesktop.FileManager1.ShowItemProperties "['$item_uri']" ""
property_window=$(timeout 10s xdotool search --sync --onlyvisible --name '^JWM Files · Properties$' | sed -n '1p')
[[ $(xdotool getwindowpid "$property_window") == "$first" ]]
printf 'GTK4 X11 FileManager1: ShowItemProperties mapped a native panel without convenience activation\n'

BUS
